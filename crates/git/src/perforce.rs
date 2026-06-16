//! Perforce backend implementing the [`GitRepository`] trait.
//!
//! This lets Zed's existing version-control UI (project-panel gutter, git panel,
//! buffer diffs) render Perforce workspace state with no UI changes: the backend
//! just populates the same [`GitStatus`]/[`FileStatus`] types the git backend does.
//!
//! Design notes:
//! - Mirrors [`crate::repository::RealGitRepository`]: shells out to the `p4` binary,
//!   every command runs on the [`BackgroundExecutor`] so the main thread never blocks.
//! - MVP scope is **read-only status**. Mutating/history methods are stubbed to return
//!   an "unsupported" error (never panic) until later phases wire them up.
//! - Connection parameters (P4PORT/P4CLIENT/P4USER) are **never hardcoded**. We run with
//!   `cwd` = workspace root and inherit the project environment, so `p4` resolves them
//!   from `P4CONFIG` / `p4 set` / the ticket cache exactly like the user's shell does.
//! - Every `p4` invocation here is verified against the vscode-perforce extension's
//!   `src/api/commands/*.ts` for flag/output correctness.

use crate::blame::Blame;
use crate::repository::{
    AskPassDelegate, BranchesScanResult, CommitDetails, CommitDiff, CommitOptions, CreateWorktreeTarget,
    DiffType, FetchOptions, GitCommitTemplate, GitRepository, GitRepositoryCheckpoint,
    PushOptions, RemoteCommandOutput, RepoPath, ResetMode,
};
use crate::{Oid, RunHook};
use crate::stash::GitStash;
use crate::status::{DiffTreeType, FileStatus, GitStatus, StatusCode, TrackedStatus, TreeDiff};
use anyhow::{Context as _, Result};
use collections::HashMap;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use gpui::{AsyncApp, BackgroundExecutor, SharedString, Task};
use rope::Rope;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use text::LineEnding;
use util::command::new_command;
use util::rel_path::RelPath;

const UNSUPPORTED: &str = "operation not supported by the Perforce backend yet";

/// The configured `P4CONFIG` marker filename(s), resolved once from `p4 set P4CONFIG`.
///
/// The name is NOT fixed: it is a machine/registry/env-scoped Perforce setting and varies
/// by site and platform (commonly `.p4config`, but also e.g. `p4config.txt`). We must honor
/// whatever the user configured rather than assume a constant. Until resolved, callers fall
/// back to the [`crate::P4CONFIG`] default.
static P4CONFIG_MARKERS: OnceLock<Vec<String>> = OnceLock::new();

/// Documented platform variants of the `P4CONFIG` filename, used before the configured
/// value is resolved. Covers the common cross-platform names so discovery is not lost to
/// the startup race (the worktree scan hits the root marker before `p4 set P4CONFIG`
/// resolves). The resolved value overrides this for any exotic custom name.
pub const DEFAULT_P4CONFIG_MARKERS: &[&str] = &[".p4config", "p4config.txt"];

/// Returns true if `name` is a configured Perforce config-file marker.
///
/// Cheap and allocation-free; safe to call from the worktree scan hot path. Before
/// resolution completes it matches the [`DEFAULT_P4CONFIG_MARKERS`] common set, so the
/// realistic platform names are recognized even during the startup race.
pub fn is_p4_config_name(name: &str) -> bool {
    match P4CONFIG_MARKERS.get() {
        Some(names) => names.iter().any(|n| n == name),
        None => DEFAULT_P4CONFIG_MARKERS.contains(&name),
    }
}

/// Record the resolved marker name(s). Idempotent (first writer wins).
pub fn set_p4_config_marker_names(names: Vec<String>) {
    if names.is_empty() {
        return;
    }
    let _ = P4CONFIG_MARKERS.set(names);
}

/// Parse the value out of `p4 set P4CONFIG` output.
///
/// Examples:
/// - `P4CONFIG=.p4config (set) (config 'noconfig')` -> `Some(".p4config")`
/// - `P4CONFIG=p4config.txt (set -s)`               -> `Some("p4config.txt")`
/// - `P4CONFIG= (config 'noconfig')` / empty        -> `None` (unset)
///
/// Mirrors the p4-toolkit idiom `(p4 set VAR) -replace "VAR=","" -replace "\s*\(set\).*",""`.
fn parse_p4_config_setting(output: &str) -> Option<String> {
    let line = output.lines().find(|l| l.starts_with("P4CONFIG="))?;
    let rest = line.strip_prefix("P4CONFIG=")?;
    // The value is the token up to the first space; unset yields an empty token.
    let name = rest.split(' ').next().unwrap_or("");
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Resolve the configured `P4CONFIG` marker filename via `p4 set P4CONFIG`.
///
/// Returns the configured name, or the [`crate::P4CONFIG`] default when P4CONFIG is unset
/// (so file-marker discovery still works for the common case). Runs on the background
/// executor; intended to be called once at startup to seed [`P4CONFIG_MARKERS`].
pub async fn resolve_p4_config_marker_names(
    p4_binary_path: PathBuf,
    envs: Arc<HashMap<String, String>>,
    working_directory: PathBuf,
    executor: BackgroundExecutor,
) -> Vec<String> {
    let cli = P4Cli {
        p4_binary_path,
        working_directory,
        executor,
        envs,
    };
    match cli.run(false, &["set", "P4CONFIG"]).await {
        Ok(out) => match parse_p4_config_setting(&out) {
            Some(name) => vec![name],
            None => vec![crate::P4CONFIG.to_string()],
        },
        Err(_) => vec![crate::P4CONFIG.to_string()],
    }
}

/// Async wrapper around the `p4` command-line client.
///
/// Analogous to [`crate::repository::RealGitRepository`]'s `GitBinary`. Spawns a fresh
/// `p4` process per command (MVP); a persistent long-lived connection is a later
/// optimization. `cwd` is pinned to the workspace root so connection settings resolve
/// from `P4CONFIG`/`p4 set`; only the inherited project environment is passed.
#[derive(Clone)]
pub(crate) struct P4Cli {
    p4_binary_path: PathBuf,
    working_directory: PathBuf,
    executor: BackgroundExecutor,
    envs: Arc<HashMap<String, String>>,
}

impl P4Cli {
    fn build_command<S: AsRef<OsStr>>(&self, tagged: bool, args: &[S]) -> util::command::Command {
        let mut command = new_command(&self.p4_binary_path);
        command.current_dir(&self.working_directory);
        // `-ztag` is a global option and must precede the subcommand. It produces
        // machine-readable tagged output (`... key value`); see `parse_ztag`.
        if tagged {
            command.arg("-ztag");
        }
        command.args(args);
        command.envs(self.envs.as_ref());
        command
    }

    /// Run a `p4` command, erroring on a nonzero exit status.
    async fn run<S: AsRef<OsStr>>(&self, tagged: bool, args: &[S]) -> Result<String> {
        let (stdout, stderr, ok) = self.run_lenient(tagged, args).await?;
        anyhow::ensure!(ok, "p4 command failed: {stderr}");
        Ok(stdout)
    }

    /// Run a `p4` command, tolerating a nonzero exit status.
    ///
    /// Several read commands (`p4 opened`, `p4 fstat`) return nonzero with informational
    /// stderr like "file(s) not opened on this client" — which is not an error for us.
    /// Returns `(stdout, stderr, status.success())`.
    async fn run_lenient<S: AsRef<OsStr>>(
        &self,
        tagged: bool,
        args: &[S],
    ) -> Result<(String, String, bool)> {
        let mut command = self.build_command(tagged, args);
        let output = command.output().await.context("spawning p4")?;
        Ok((
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            output.status.success(),
        ))
    }
}

/// Parse `p4 -ztag` / `p4 fstat` tagged output into a list of records.
///
/// Tagged output is a sequence of `... key value` lines; a blank line separates records.
/// A key with no value (a flag field) maps to an empty string. Matches the vscode-perforce
/// parser in `src/api/commands/fstat.ts` (`/[.]{3} (\w+)[ ]*(.+)?/`).
pub(crate) fn parse_ztag(output: &str) -> Vec<HashMap<String, String>> {
    let mut records = Vec::new();
    let mut current: HashMap<String, String> = HashMap::default();
    for raw in output.lines() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            if !current.is_empty() {
                records.push(std::mem::take(&mut current));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("... ") {
            let mut parts = rest.splitn(2, ' ');
            let key = parts.next().unwrap_or_default();
            let value = parts.next().unwrap_or_default();
            if !key.is_empty() {
                current.insert(key.to_string(), value.to_string());
            }
        }
        // Lines not starting with "... " are continuations of a multi-line value
        // (uncommon in the fields we read); ignored for MVP.
    }
    if !current.is_empty() {
        records.push(current);
    }
    records
}

/// Map a `p4 opened`/`fstat` action verb to a [`StatusCode`].
///
/// Verified against vscode-perforce `src/scm/Status.ts`. We record the Perforce "opened"
/// state as a worktree (unstaged) change for MVP; the index/changelist split lands later.
fn action_to_status(action: &str) -> Option<FileStatus> {
    let code = match action {
        "add" | "branch" | "import" | "move/add" => StatusCode::Added,
        "delete" | "move/delete" | "purge" => StatusCode::Deleted,
        "edit" | "integrate" | "archive" => StatusCode::Modified,
        _ => return None,
    };
    Some(FileStatus::Tracked(TrackedStatus {
        index_status: StatusCode::Unmodified,
        worktree_status: code,
    }))
}

/// Convert a `p4 opened` `clientFile` (client syntax, e.g. `//client/a/b.txt`) into a
/// repo-relative [`RepoPath`]. `p4 -ztag opened` always reports `clientFile` rooted at the
/// client name with forward slashes — so we strip `//{client}/` rather than touching the
/// local filesystem path (which `opened` does not give us).
fn client_path_to_repo_path(client_name: &str, client_file: &str) -> Option<RepoPath> {
    let prefix = format!("//{client_name}/");
    let rel = client_file.strip_prefix(&prefix)?;
    let rel_path = RelPath::unix(rel).ok()?;
    Some(RepoPath::from_rel_path(&rel_path))
}

/// Build a [`GitStatus`] from `p4 -ztag opened` output.
///
/// MVP strategy: `p4 opened` lists exactly the files open in this client (it does not scan
/// the whole tree). Each record's `action` becomes a [`FileStatus`] and its `clientFile`
/// becomes a [`RepoPath`].
fn parse_opened_status(client_name: &str, opened_output: &str) -> GitStatus {
    let mut entries: Vec<(RepoPath, FileStatus)> = Vec::new();
    for record in parse_ztag(opened_output) {
        let Some(action) = record.get("action") else {
            continue;
        };
        let Some(status) = action_to_status(action) else {
            continue;
        };
        let Some(client_file) = record.get("clientFile") else {
            continue;
        };
        let Some(repo_path) = client_path_to_repo_path(client_name, client_file) else {
            continue;
        };
        entries.push((repo_path, status));
    }
    entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    entries.dedup_by(|(a, _), (b, _)| a == b);
    GitStatus {
        entries: entries.into(),
    }
}

/// A discovered Perforce workspace: where it lives locally and its client name.
#[derive(Clone, Debug)]
pub struct PerforceWorkspace {
    /// Local root of the client (from `p4 info`'s `clientRoot`).
    pub client_root: PathBuf,
    /// Client (workspace) name (`p4 info`'s `clientName`).
    pub client_name: String,
}

/// A Perforce-backed repository.
pub struct PerforceRepository {
    cli: P4Cli,
    /// Local workspace root (client root).
    working_directory: PathBuf,
    client_name: String,
    is_trusted: Arc<AtomicBool>,
}

impl PerforceRepository {
    /// Construct a backend for an already-detected workspace.
    pub fn new(
        p4_binary_path: PathBuf,
        workspace: PerforceWorkspace,
        envs: Arc<HashMap<String, String>>,
        executor: BackgroundExecutor,
    ) -> Self {
        let working_directory = workspace.client_root;
        let cli = P4Cli {
            p4_binary_path,
            working_directory: working_directory.clone(),
            executor,
            envs,
        };
        Self {
            cli,
            working_directory,
            client_name: workspace.client_name,
            is_trusted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Detect whether `dir` lives inside a Perforce workspace by asking `p4 info`.
    ///
    /// Runs with `cwd = dir` so connection settings resolve from `P4CONFIG`/`p4 set`.
    /// Returns the workspace if `clientName` is set and `clientRoot` is a real path that
    /// contains `dir`. Never hardcodes any connection value.
    pub async fn detect(
        p4_binary_path: PathBuf,
        dir: &Path,
        envs: Arc<HashMap<String, String>>,
        executor: BackgroundExecutor,
    ) -> Option<PerforceWorkspace> {
        let probe = P4Cli {
            p4_binary_path,
            working_directory: dir.to_path_buf(),
            executor,
            envs,
        };
        let (stdout, _stderr, ok) = probe.run_lenient(true, &["info"]).await.ok()?;
        if !ok {
            return None;
        }
        let info = parse_ztag(&stdout).into_iter().next()?;
        let client_name = info.get("clientName")?.trim().to_string();
        let client_root = info.get("clientRoot")?.trim().to_string();
        if client_name.is_empty()
            || client_name == "*unknown*"
            || client_root.is_empty()
            || client_root == "*unknown*"
        {
            return None;
        }
        log::info!(
            "perforce: detected workspace client={client_name} root={client_root} at {dir:?}"
        );
        Some(PerforceWorkspace {
            client_root: PathBuf::from(client_root),
            client_name,
        })
    }

    /// Client-syntax path for a repo-relative file, e.g. `//client/a/b.txt`.
    fn client_syntax_path(&self, path: &RepoPath) -> String {
        format!("//{}/{}", self.client_name, path.as_unix_str())
    }
}

/// Stub a `BoxFuture<Result<T>>` trait method as unsupported. Returns `Err`, so the
/// concrete `T` never has to be constructed.
macro_rules! unsupported_result {
    () => {
        async { anyhow::bail!(UNSUPPORTED) }.boxed()
    };
}

impl GitRepository for PerforceRepository {
    fn status(&self, path_prefixes: &[RepoPath]) -> Task<Result<GitStatus>> {
        // `p4 opened` lists exactly the files open in this client. A full listing is
        // always correct (the caller merges per-path), so scoping to the changed paths is
        // a pure optimization: for small incremental refreshes (the steady state — user
        // edits a handful of files) we restrict the query, avoiding a full re-list on
        // every keystroke. A large set (e.g. the initial scan) falls back to one full
        // listing, which is cheaper than batching hundreds of path arguments.
        //
        // NOTE: read-only-bit pre-filtering and a persistent `p4` connection are further
        // optimizations tracked for later.
        const SCOPED_CAP: usize = 64;
        let cli = self.cli.clone();
        let client_name = self.client_name.clone();
        let n_prefixes = path_prefixes.len();
        let scoped: Option<Vec<String>> = if (1..=SCOPED_CAP).contains(&n_prefixes) {
            Some(
                path_prefixes
                    .iter()
                    .map(|p| self.client_syntax_path(p))
                    .collect(),
            )
        } else {
            None
        };
        self.cli.executor.clone().spawn(async move {
            let mut args: Vec<String> = vec!["opened".into()];
            let is_scoped = scoped.is_some();
            if let Some(paths) = scoped {
                args.extend(paths);
            }
            let (stdout, _stderr, _ok) = cli.run_lenient(true, &args).await?;
            let status = parse_opened_status(&client_name, &stdout);
            log::debug!(
                "perforce: status client={client_name} scoped={is_scoped} prefixes={n_prefixes} -> {} entries",
                status.entries.len()
            );
            Ok(status)
        })
    }

    fn load_committed_text(&self, path: RepoPath) -> BoxFuture<'_, Option<String>> {
        // Depot content of the currently-synced revision: `p4 print -q //client/path#have`.
        let cli = self.cli.clone();
        let arg = format!("{}#have", self.client_syntax_path(&path));
        async move {
            let out = cli.run(false, &["print", "-q", &arg]).await.ok()?;
            Some(out)
        }
        .boxed()
    }

    fn path(&self) -> PathBuf {
        self.working_directory.clone()
    }

    fn main_repository_path(&self) -> PathBuf {
        self.working_directory.clone()
    }

    fn set_trusted(&self, trusted: bool) {
        self.is_trusted.store(trusted, Ordering::SeqCst);
    }

    fn is_trusted(&self) -> bool {
        self.is_trusted.load(Ordering::SeqCst)
    }

    // ---- Everything below is unsupported in the MVP (read-only status only). ----

    fn load_index_text(&self, _path: RepoPath) -> BoxFuture<'_, Option<String>> {
        async { None }.boxed()
    }

    fn load_blob_content(&self, _oid: Oid) -> BoxFuture<'_, Result<String>> {
        unsupported_result!()
    }

    fn set_index_text(
        &self,
        _path: RepoPath,
        _content: Option<String>,
        _env: Arc<HashMap<String, String>>,
        _is_executable: bool,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn remote_urls(&self) -> BoxFuture<'_, HashMap<String, String>> {
        async { HashMap::default() }.boxed()
    }

    fn revparse_batch(&self, revs: Vec<String>) -> BoxFuture<'_, Result<Vec<Option<String>>>> {
        async move { Ok(revs.into_iter().map(|_| None).collect()) }.boxed()
    }

    fn merge_message(&self) -> BoxFuture<'_, Option<String>> {
        async { None }.boxed()
    }

    fn diff_tree(&self, _request: DiffTreeType) -> BoxFuture<'_, Result<TreeDiff>> {
        unsupported_result!()
    }

    fn stash_entries(&self) -> BoxFuture<'static, Result<GitStash>> {
        async { Ok(GitStash::default()) }.boxed()
    }

    fn branches(&self) -> BoxFuture<'_, Result<BranchesScanResult>> {
        unsupported_result!()
    }

    fn change_branch(&self, _name: String) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn create_branch(
        &self,
        _name: String,
        _base_branch: Option<String>,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn rename_branch(&self, _branch: String, _new_name: String) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn delete_branch(
        &self,
        _is_remote: bool,
        _name: String,
        _force: bool,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn worktrees(&self) -> BoxFuture<'_, Result<Vec<crate::repository::Worktree>>> {
        async { Ok(Vec::new()) }.boxed()
    }

    fn create_worktree(
        &self,
        _target: CreateWorktreeTarget,
        _path: PathBuf,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn checkout_branch_in_worktree(
        &self,
        _branch_name: String,
        _worktree_path: PathBuf,
        _create: bool,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn remove_worktree(&self, _path: PathBuf, _force: bool) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn rename_worktree(&self, _old_path: PathBuf, _new_path: PathBuf) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn reset(
        &self,
        _commit: String,
        _mode: ResetMode,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn checkout_files(
        &self,
        _commit: String,
        _paths: Vec<RepoPath>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn show(&self, _commit: String) -> BoxFuture<'_, Result<CommitDetails>> {
        unsupported_result!()
    }

    fn load_commit(&self, _commit: String, _cx: AsyncApp) -> BoxFuture<'_, Result<CommitDiff>> {
        unsupported_result!()
    }

    fn blame(
        &self,
        _path: RepoPath,
        _content: Rope,
        _line_ending: LineEnding,
    ) -> BoxFuture<'_, Result<Blame>> {
        unsupported_result!()
    }

    fn stage_paths(
        &self,
        _paths: Vec<RepoPath>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn unstage_paths(
        &self,
        _paths: Vec<RepoPath>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn run_hook(
        &self,
        _hook: RunHook,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn commit(
        &self,
        _message: SharedString,
        _name_and_email: Option<(SharedString, SharedString)>,
        _options: CommitOptions,
        _askpass: AskPassDelegate,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn stash_paths(
        &self,
        _paths: Vec<RepoPath>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn stash_pop(
        &self,
        _index: Option<usize>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn stash_apply(
        &self,
        _index: Option<usize>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn stash_drop(
        &self,
        _index: Option<usize>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn push(
        &self,
        _branch_name: String,
        _remote_branch_name: String,
        _upstream_name: String,
        _options: Option<PushOptions>,
        _askpass: AskPassDelegate,
        _env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        unsupported_result!()
    }

    fn pull(
        &self,
        _branch_name: Option<String>,
        _upstream_name: String,
        _rebase: bool,
        _askpass: AskPassDelegate,
        _env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        unsupported_result!()
    }

    fn fetch(
        &self,
        _fetch_options: FetchOptions,
        _askpass: AskPassDelegate,
        _env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        unsupported_result!()
    }

    fn get_push_remote(
        &self,
        _branch: String,
    ) -> BoxFuture<'_, Result<Option<crate::repository::Remote>>> {
        async { Ok(None) }.boxed()
    }

    fn get_branch_remote(
        &self,
        _branch: String,
    ) -> BoxFuture<'_, Result<Option<crate::repository::Remote>>> {
        async { Ok(None) }.boxed()
    }

    fn get_all_remotes(&self) -> BoxFuture<'_, Result<Vec<crate::repository::Remote>>> {
        async { Ok(Vec::new()) }.boxed()
    }

    fn remove_remote(&self, _name: String) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn create_remote(&self, _name: String, _url: String) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn check_for_pushed_commit(&self) -> BoxFuture<'_, Result<Vec<SharedString>>> {
        async { Ok(Vec::new()) }.boxed()
    }

    fn diff(&self, _diff: DiffType) -> BoxFuture<'_, Result<String>> {
        unsupported_result!()
    }

    fn diff_stat(
        &self,
        _path_prefixes: &[RepoPath],
    ) -> BoxFuture<'static, Result<crate::status::GitDiffStat>> {
        async { Ok(crate::status::GitDiffStat::default()) }.boxed()
    }

    fn checkpoint(&self) -> BoxFuture<'static, Result<GitRepositoryCheckpoint>> {
        unsupported_result!()
    }

    fn restore_checkpoint(
        &self,
        _checkpoint: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn create_archive_checkpoint(&self) -> BoxFuture<'_, Result<(String, String)>> {
        unsupported_result!()
    }

    fn restore_archive_checkpoint(
        &self,
        _staged_sha: String,
        _unstaged_sha: String,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn compare_checkpoints(
        &self,
        _left: GitRepositoryCheckpoint,
        _right: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<bool>> {
        unsupported_result!()
    }

    fn diff_checkpoints(
        &self,
        _base_checkpoint: GitRepositoryCheckpoint,
        _target_checkpoint: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<String>> {
        unsupported_result!()
    }

    fn load_commit_template(&self) -> BoxFuture<'_, Result<Option<GitCommitTemplate>>> {
        async { Ok(None) }.boxed()
    }

    fn default_branch(
        &self,
        _include_remote_name: bool,
    ) -> BoxFuture<'_, Result<Option<SharedString>>> {
        async { Ok(None) }.boxed()
    }

    fn initial_graph_data(
        &self,
        _log_source: crate::repository::LogSource,
        _log_order: crate::repository::LogOrder,
        _request_tx: async_channel::Sender<Vec<Arc<crate::repository::InitialGraphCommitData>>>,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn search_commits(
        &self,
        _log_source: crate::repository::LogSource,
        _search_args: crate::repository::SearchCommitArgs,
        _request_tx: async_channel::Sender<Oid>,
    ) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn file_history_changed_files(
        &self,
        _paths: Vec<RepoPath>,
        _commit_limit: usize,
    ) -> BoxFuture<'_, Result<Vec<crate::repository::FileHistoryChangedFileSets>>> {
        unsupported_result!()
    }

    fn commit_data_reader(&self) -> Result<crate::repository::CommitDataReader> {
        anyhow::bail!(UNSUPPORTED)
    }

    fn update_ref(&self, _ref_name: String, _commit: String) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn delete_ref(&self, _ref_name: String) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }

    fn repair_worktrees(&self) -> BoxFuture<'_, Result<()>> {
        unsupported_result!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::FileStatus;

    // Real `p4 -ztag info` output captured from a live server (client/server/user
    // identifiers are environment values, used here only as parser fixtures).
    const INFO_FIXTURE: &str = "\
... userName someuser
... clientName some_client_name
... clientRoot E:/Projects\\some_client_name
... clientStream //Depot.Project/Release/branch_x
... clientCase insensitive
";

    // Real `p4 -ztag opened` output shape: client-syntax `clientFile`, blank-line records.
    const OPENED_FIXTURE: &str = "\
... depotFile //Depot.Project/Release/branch_x/a/added.md
... clientFile //some_client_name/a/added.md
... rev 1
... action add
... change 6596347
... type text

... depotFile //Depot.Project/Release/branch_x/b/edited.cpp
... clientFile //some_client_name/b/edited.cpp
... rev 3
... action edit
... change default
... type text

... depotFile //Depot.Project/Release/branch_x/c/gone.txt
... clientFile //some_client_name/c/gone.txt
... rev 2
... action delete
... change 6596347
... type text
";

    #[test]
    fn ztag_splits_records_on_blank_lines() {
        let records = parse_ztag(INFO_FIXTURE);
        assert_eq!(records.len(), 1);
        let info = &records[0];
        assert_eq!(info.get("clientName").unwrap(), "some_client_name");
        assert_eq!(info.get("clientRoot").unwrap(), "E:/Projects\\some_client_name");
        // Flag-style field with no value still parses (here all have values).
        assert_eq!(info.get("clientCase").unwrap(), "insensitive");
    }

    #[test]
    fn ztag_handles_valueless_field() {
        let records = parse_ztag("... mapped\n... depotFile //x/y\n");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].get("mapped").unwrap(), "");
        assert_eq!(records[0].get("depotFile").unwrap(), "//x/y");
    }

    #[test]
    fn client_path_strips_client_prefix() {
        let p = client_path_to_repo_path("some_client_name", "//some_client_name/a/added.md")
            .expect("should map");
        assert_eq!(p.as_unix_str(), "a/added.md");
    }

    #[test]
    fn client_path_rejects_foreign_client() {
        assert!(client_path_to_repo_path("some_client_name", "//other_client/a/b.md").is_none());
    }

    #[test]
    fn opened_status_maps_actions() {
        let status = parse_opened_status("some_client_name", OPENED_FIXTURE);
        let map: HashMap<String, FileStatus> = status
            .entries
            .iter()
            .map(|(p, s)| (p.as_unix_str().to_string(), *s))
            .collect();
        assert_eq!(map.len(), 3);
        assert!(map["a/added.md"].is_created());
        assert!(map["b/edited.cpp"].is_modified());
        assert!(map["c/gone.txt"].is_deleted());
    }

    #[test]
    fn parse_p4config_setting_variants() {
        assert_eq!(
            parse_p4_config_setting("P4CONFIG=.p4config (set) (config 'noconfig')").as_deref(),
            Some(".p4config")
        );
        assert_eq!(
            parse_p4_config_setting("P4CONFIG=p4config.txt (set -s)").as_deref(),
            Some("p4config.txt")
        );
        // Unset: empty value before the annotation.
        assert_eq!(
            parse_p4_config_setting("P4CONFIG= (config 'noconfig')"),
            None
        );
        // Bare value, no annotation.
        assert_eq!(
            parse_p4_config_setting("P4CONFIG=.myp4cfg").as_deref(),
            Some(".myp4cfg")
        );
        // No P4CONFIG line at all.
        assert_eq!(parse_p4_config_setting("P4PORT=ssl:host:1666\n"), None);
    }

    #[test]
    fn is_p4_config_name_defaults_before_resolution() {
        // Before resolution the documented platform variants both match (avoids losing
        // discovery to the startup race); an exotic custom name does not.
        assert!(is_p4_config_name(".p4config"));
        assert!(is_p4_config_name("p4config.txt"));
        assert!(!is_p4_config_name("some_custom_name"));
        assert!(!is_p4_config_name(".gitignore"));
    }

    #[test]
    fn action_mapping_known_verbs() {
        assert!(action_to_status("add").unwrap().is_created());
        assert!(action_to_status("branch").unwrap().is_created());
        assert!(action_to_status("move/add").unwrap().is_created());
        assert!(action_to_status("edit").unwrap().is_modified());
        assert!(action_to_status("integrate").unwrap().is_modified());
        assert!(action_to_status("delete").unwrap().is_deleted());
        assert!(action_to_status("move/delete").unwrap().is_deleted());
        assert!(action_to_status("nonsense").is_none());
    }
}
