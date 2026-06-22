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

use crate::blame::{Blame, BlameEntry};
use crate::repository::{
    AskPassDelegate, BranchesScanResult, CommitData, CommitDataReader, CommitDetails, CommitDiff,
    CommitFile, CommitOptions, CreateWorktreeTarget, DiffType, FetchOptions, GitCommitTemplate,
    GitRepository, GitRepositoryCheckpoint, InitialGraphCommitData, LogOrder, LogSource,
    PushOptions, RemoteCommandOutput, RepoPath, ResetMode,
};
use crate::{Oid, RunHook};
use crate::stash::GitStash;
use crate::status::{DiffTreeType, FileStatus, GitStatus, StatusCode, TrackedStatus, TreeDiff};
use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use futures::FutureExt as _;
use futures::future::BoxFuture;
use gpui::{AsyncApp, BackgroundExecutor, SharedString, Task};
use parking_lot::Mutex;
use rope::Rope;
use smallvec::SmallVec;
use std::str::FromStr as _;
use std::time::SystemTime;
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
        // Lines not starting with "... " are continuations of a multi-line value; the generic
        // single-line consumers (opened/fstat/info, and blame which takes only the first desc
        // line) don't need them. Multi-line changelist descriptions use `parse_pending_changes`.
    }
    if !current.is_empty() {
        records.push(current);
    }
    records
}

/// One pending changelist as reported by `p4 changes -s pending -l`.
struct PendingChange {
    number: u32,
    description: String,
    /// `changes -l` set the `... shelved` flag — this changelist has shelved files to fetch.
    has_shelved: bool,
}

/// Parse `p4 -ztag changes -s pending -l` into `(change number, full description)` pairs.
///
/// Why not [`parse_ztag`]: a changelist's `desc` is a **multi-line** value that itself contains
/// blank lines (e.g. a paragraph break), so the generic "blank line = record boundary" rule would
/// truncate the description and split one changelist into bogus records. Here we instead treat
/// each `... change N` line as the start of a record and append every non-`... ` line (blank
/// lines included) to the `desc` value, until the next `... <field>` ends it. This is robust to
/// `desc` appearing last (as in `changes`) or mid-record (as in `describe`).
fn parse_pending_changes(output: &str) -> Vec<PendingChange> {
    let mut out: Vec<PendingChange> = Vec::new();
    let mut change: Option<u32> = None;
    let mut desc = String::new();
    let mut shelved = false;
    let mut in_desc = false;

    for raw in output.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("... ") {
            let mut parts = rest.splitn(2, ' ');
            let key = parts.next().unwrap_or_default();
            let value = parts.next().unwrap_or_default();
            match key {
                "change" => {
                    if let Some(number) = change.take() {
                        out.push(PendingChange {
                            number,
                            description: std::mem::take(&mut desc).trim().to_string(),
                            has_shelved: shelved,
                        });
                    }
                    desc.clear();
                    shelved = false;
                    change = value.parse::<u32>().ok();
                    in_desc = false;
                }
                // `changes -l` reports a `... shelved` flag field for changelists that have
                // shelved files; presence (any value) means there are shelves to fetch.
                "shelved" => {
                    shelved = true;
                    in_desc = false;
                }
                "desc" => {
                    desc.push_str(value);
                    in_desc = true;
                }
                _ => in_desc = false,
            }
        } else if in_desc {
            // Continuation line (a blank line included) belongs to the multi-line description.
            desc.push('\n');
            desc.push_str(line);
        }
    }
    if let Some(number) = change.take() {
        out.push(PendingChange {
            number,
            description: desc.trim().to_string(),
            has_shelved: shelved,
        });
    }
    out
}

/// Parse `p4 -ztag opened` output into a map of repo path -> action verb (`add`, `edit`,
/// `delete`, `move/add`, ...). Used by delete handling to choose revert vs delete.
fn parse_opened_actions(client_name: &str, opened_output: &str) -> HashMap<RepoPath, String> {
    let mut map = HashMap::default();
    for record in parse_ztag(opened_output) {
        if let (Some(client_file), Some(action)) = (record.get("clientFile"), record.get("action"))
            && let Some(repo_path) = client_path_to_repo_path(client_name, client_file)
        {
            map.insert(repo_path, action.clone());
        }
    }
    map
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

/// Convert an `fstat`-style `clientFile` — which is a **local filesystem path** (e.g.
/// `E:/Projects\client\a\b.txt`), unlike `opened`'s client-syntax path — into a repo-relative
/// [`RepoPath`] by stripping the client root. Separators are normalized to `/` first (p4 mixes
/// `/` and `\` on Windows) and the prefix match is case-insensitive (Windows paths).
fn local_path_to_repo_path(working_directory: &Path, client_file: &str) -> Option<RepoPath> {
    let root = working_directory.to_string_lossy().replace('\\', "/");
    let root = root.trim_end_matches('/');
    let file = client_file.replace('\\', "/");
    if file.len() < root.len() || !file[..root.len()].eq_ignore_ascii_case(root) {
        return None;
    }
    let rel = file[root.len()..].trim_start_matches('/');
    if rel.is_empty() {
        return None;
    }
    let rel_path = RelPath::unix(rel).ok()?;
    Some(RepoPath::from_rel_path(&rel_path))
}

/// Parse `p4 -ztag fstat -Rs -e <cl> //client/...` (shelved files of one changelist) into
/// `(change number, file)` pairs. Each shelved file reports a local `clientFile`, an `action`,
/// and its `change`; records lacking those (e.g. the trailing changelist-description record) are
/// skipped. Every returned file is marked `shelved`.
fn parse_shelved_files(
    working_directory: &Path,
    fstat_output: &str,
) -> Vec<(u32, ChangelistFile)> {
    let mut out = Vec::new();
    for record in parse_ztag(fstat_output) {
        let (Some(client_file), Some(action), Some(change)) = (
            record.get("clientFile"),
            record.get("action"),
            record.get("change").and_then(|c| c.parse::<u32>().ok()),
        ) else {
            continue;
        };
        let (Some(status), Some(path)) = (
            action_to_status(action),
            local_path_to_repo_path(working_directory, client_file),
        ) else {
            continue;
        };
        out.push((
            change,
            ChangelistFile {
                path,
                status,
                shelved: true,
            },
        ));
    }
    out
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

/// Identifies a pending changelist for the Changes panel: the special default changelist, or a
/// numbered one. The grouping puts [`ChangelistId::Default`] first, then numbered changelists in
/// descending order (newest first), matching P4V.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangelistId {
    Default,
    Numbered(u32),
}

impl ChangelistId {
    /// The `-c` argument value `p4` expects for this changelist (`default` or the number).
    fn as_p4_arg(self) -> String {
        match self {
            ChangelistId::Default => "default".to_string(),
            ChangelistId::Numbered(n) => n.to_string(),
        }
    }
}

/// One file open (or shelved) in a changelist, for the Changes panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelistFile {
    pub path: RepoPath,
    pub status: FileStatus,
    pub shelved: bool,
}

/// A pending changelist with its files, for the Changes panel grouping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerforceChangelist {
    pub id: ChangelistId,
    pub description: String,
    pub files: Vec<ChangelistFile>,
}

/// Parse a `p4 opened`/`changes` `change` field (`"default"` or a decimal number).
fn parse_change_field(field: &str) -> Option<ChangelistId> {
    if field == "default" {
        Some(ChangelistId::Default)
    } else {
        field.parse::<u32>().ok().map(ChangelistId::Numbered)
    }
}

/// Aggregate `p4 -ztag opened` (files + their `change`) and `p4 -ztag changes -s pending -l`
/// (changelist numbers + descriptions) into the grouped model the Changes panel renders.
///
/// Why this instead of P4V's per-changelist `fstat` for opened files (N+1): one `opened` call
/// already reports every open file with its `change` field, so a single aggregation pass buckets
/// them. `changes` is only needed for the descriptions and to surface *empty* pending changelists
/// (which have no `opened` files). Shelved files ARE a separate per-changelist `fstat -Rs` axis
/// (passed in via `shelved`), appended after the opened files of each changelist. The default
/// changelist is always emitted first; numbered changelists follow in descending order.
fn build_changelists(
    client_name: &str,
    opened_output: &str,
    changes: &[PendingChange],
    shelved: Vec<(u32, ChangelistFile)>,
) -> Vec<PerforceChangelist> {
    // Numbered changelists + (possibly multi-line) descriptions from `changes -s pending`.
    let mut descriptions: HashMap<u32, String> = HashMap::default();
    for change in changes {
        descriptions
            .entry(change.number)
            .or_insert_with(|| change.description.clone());
    }

    // Bucket opened files by their changelist.
    let mut files: HashMap<ChangelistId, Vec<ChangelistFile>> = HashMap::default();
    for record in parse_ztag(opened_output) {
        let (Some(action), Some(change_field), Some(client_file)) = (
            record.get("action"),
            record.get("change"),
            record.get("clientFile"),
        ) else {
            continue;
        };
        let (Some(status), Some(id), Some(path)) = (
            action_to_status(action),
            parse_change_field(change_field),
            client_path_to_repo_path(client_name, client_file),
        ) else {
            continue;
        };
        files.entry(id).or_default().push(ChangelistFile {
            path,
            status,
            shelved: false,
        });
    }

    // Shelved files always belong to a numbered changelist (you cannot shelve in the default).
    for (change, file) in shelved {
        files
            .entry(ChangelistId::Numbered(change))
            .or_default()
            .push(file);
    }

    // Numbered changelist set = union of those reported by `changes` and any referenced by an
    // opened or shelved file (defensive: never silently drop a file whose changelist was missed).
    let mut numbered: Vec<u32> = descriptions.keys().copied().collect();
    for id in files.keys() {
        if let ChangelistId::Numbered(n) = id
            && !descriptions.contains_key(n)
        {
            numbered.push(*n);
        }
    }
    numbered.sort_unstable_by(|a, b| b.cmp(a));
    numbered.dedup();

    // Within a changelist: opened files first, then shelved, each group sorted by path.
    let take_sorted = |files: &mut HashMap<ChangelistId, Vec<ChangelistFile>>, id: ChangelistId| {
        let mut v = files.remove(&id).unwrap_or_default();
        v.sort_by(|a, b| (a.shelved, &a.path).cmp(&(b.shelved, &b.path)));
        v
    };

    let mut result = Vec::with_capacity(numbered.len() + 1);
    result.push(PerforceChangelist {
        id: ChangelistId::Default,
        description: String::new(),
        files: take_sorted(&mut files, ChangelistId::Default),
    });
    for change in numbered {
        result.push(PerforceChangelist {
            id: ChangelistId::Numbered(change),
            description: descriptions.get(&change).cloned().unwrap_or_default(),
            files: take_sorted(&mut files, ChangelistId::Numbered(change)),
        });
    }
    result
}

/// On-disk open-candidate classification of a single scoped path, used by the
/// read-only-bit pre-filter (sub-task 3a).
///
/// Perforce keeps un-opened synced files **read-only on disk**; opening a file for
/// `edit`/`add` makes it **writable**. So a read-only regular file is *definitely not
/// open* and cannot contribute to status, while anything else (writable, missing, a
/// directory, or unstattable) *might* correspond to an opened file and must be confirmed
/// against the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenCandidate {
    /// A regular file that exists and is read-only ⇒ provably not open ⇒ can be skipped.
    DefinitelyNotOpen,
    /// Writable / missing / directory / unstattable ⇒ might be open ⇒ must query `p4`.
    MaybeOpen,
}

/// Classify one local path for the read-only pre-filter.
///
/// Correctness: we only ever return [`OpenCandidate::DefinitelyNotOpen`] for a path we can
/// prove is not open — a regular file present on disk with the read-only bit set. Every
/// other case (writable file, broken/symlinked target, a directory whose children we did
/// not inspect, a path that does not exist because it may be open-for-delete, or any stat
/// error) is conservatively [`OpenCandidate::MaybeOpen`], preserving the full `p4 opened`
/// result as the source of truth.
fn classify_open_candidate(local_path: &Path) -> OpenCandidate {
    match std::fs::symlink_metadata(local_path) {
        Ok(meta) if meta.is_file() && meta.permissions().readonly() => {
            OpenCandidate::DefinitelyNotOpen
        }
        // Writable regular file, directory, symlink, or anything else: cannot rule out open.
        Ok(_) => OpenCandidate::MaybeOpen,
        // Missing (possibly open-for-delete) or stat error: cannot rule out open.
        Err(_) => OpenCandidate::MaybeOpen,
    }
}

/// Decide whether a scoped `status()` refresh can short-circuit *without* calling
/// `p4 opened`, based purely on the on-disk read-only bits of its candidate paths.
///
/// Returns `true` (skip the server) iff **every** candidate path is provably not open
/// (read-only regular file). An empty candidate list is *not* skippable — that is the
/// full-refresh / large-scope case handled by the caller, never routed here.
///
/// This is a pure optimization: a `false` here just falls through to the normal
/// `p4 opened` query, which remains authoritative.
fn can_skip_opened_query(candidates: &[OpenCandidate]) -> bool {
    !candidates.is_empty()
        && candidates
            .iter()
            .all(|c| *c == OpenCandidate::DefinitelyNotOpen)
}

/// Filter the paths that actually need a `p4 open` for `action`.
///
/// For [`P4OpenAction::Edit`], a file that is already writable on disk is already open for
/// edit (or untracked) — a second `p4 edit` is a redundant server round-trip ("currently
/// opened for edit"). So we keep only the read-only (not-yet-open) files. This makes the
/// auto-checkout hooks idempotent: the first edit opens the file, and once it is writable
/// every later edit/save skips the server. `Add`/`Delete` are not gated this way.
fn paths_to_open(
    action: P4OpenAction,
    working_directory: &Path,
    paths: Vec<RepoPath>,
) -> Vec<RepoPath> {
    if action != P4OpenAction::Edit {
        return paths;
    }
    paths
        .into_iter()
        .filter(|p| {
            classify_open_candidate(&working_directory.join(p.as_std_path()))
                == OpenCandidate::DefinitelyNotOpen
        })
        .collect()
}

/// Per-changelist metadata parsed from `p4 filelog`, keyed by change number.
#[derive(Debug, Clone, Default)]
struct ChangeMeta {
    user: Option<String>,
    /// Unix epoch seconds.
    time: Option<i64>,
    /// First line of the changelist description.
    summary: Option<String>,
}

/// Map a Perforce change number to a synthetic git [`Oid`].
///
/// `BlameEntry.sha` is a fixed-width git hash and cannot hold a decimal change number, so we
/// encode the number into the leading bytes. The blame UI shows the human-readable
/// `@change` via `revision_label`; this synthetic Oid is only used as a stable per-change
/// key (e.g. for the gutter's deterministic color and the `messages` map).
fn change_to_oid(change: u32) -> Oid {
    let mut bytes = [0u8; 20];
    bytes[..4].copy_from_slice(&change.to_be_bytes());
    Oid::from_bytes(&bytes).expect("20 bytes is a valid SHA-1 oid")
}

/// Inverse of [`change_to_oid`]: recover a change number from its synthetic Oid (the file-history
/// graph uses these Oids as commit ids, so the commit-data reader decodes them back).
fn oid_to_change(oid: &Oid) -> u32 {
    let bytes = oid.as_bytes();
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// One revision of a file from `p4 filelog -l -t <path>` (newest first).
#[derive(Debug, Clone, PartialEq, Eq)]
struct FilelogRev {
    rev: u32,
    change: u32,
    action: String,
    user: String,
    /// Unix epoch seconds (from the `-t` timestamp), if parseable.
    time: Option<i64>,
    /// Full changelist description (tab-indented lines joined), from `-l`.
    desc: String,
}

/// Parse the text output of `p4 filelog -l -t <path>` into one [`FilelogRev`] per revision.
///
/// The non-`-ztag` text form is used (mirroring vscode-perforce's `filelog.ts`) because it frames
/// the multi-line description cleanly: a depot-path header line, then one `... #<rev> change ...`
/// record per revision, with the description on following tab-indented lines. Integration records
/// (`... ... <op> from/into //depot#n`) and blank lines are ignored.
fn parse_filelog(output: &str) -> Vec<FilelogRev> {
    let mut revs: Vec<FilelogRev> = Vec::new();
    let mut desc_lines: Vec<String> = Vec::new();
    let flush = |revs: &mut Vec<FilelogRev>, desc_lines: &mut Vec<String>| {
        if let Some(last) = revs.last_mut() {
            last.desc = desc_lines.join("\n").trim_end().to_string();
        }
        desc_lines.clear();
    };
    for line in output.lines() {
        if let Some(header) = line.strip_prefix("... #") {
            flush(&mut revs, &mut desc_lines);
            if let Some(rev) = parse_filelog_header(header) {
                revs.push(rev);
            }
        } else if let Some(desc) = line.strip_prefix('\t') {
            desc_lines.push(desc.to_string());
        }
        // Everything else (depot-path header, `... ...` integration lines, blanks) is ignored.
    }
    flush(&mut revs, &mut desc_lines);
    revs
}

/// Parse a filelog header body (the part after `... #`): `<rev> change <chnum> <action> on <date>
/// by <user>@<client> (<type>)`.
fn parse_filelog_header(s: &str) -> Option<FilelogRev> {
    let (rev_str, rest) = s.split_once(" change ")?;
    let rev = rev_str.trim().parse::<u32>().ok()?;
    let (change_str, rest) = rest.split_once(' ')?;
    let change = change_str.trim().parse::<u32>().ok()?;
    // `<action> on <date...> by <user>@<client> (<type>)`. The date may include a time component
    // (with `-t`), so split on the ` on `/` by ` markers rather than counting whitespace tokens.
    let (action, rest) = rest.split_once(" on ")?;
    let (date_part, rest) = rest.split_once(" by ")?;
    let user = rest.split('@').next()?.trim().to_string();
    Some(FilelogRev {
        rev,
        change,
        action: action.trim().to_string(),
        user,
        time: parse_p4_datetime(date_part.trim()),
        desc: String::new(),
    })
}

/// Parse `p4 -ztag describe -s <change>` into the change's changed files as
/// `(depotFile, rev, action)`, read from the indexed `depotFile{i}` / `rev{i}` / `action{i}`
/// fields. Robust to a description that contains a blank line (which `parse_ztag` would split into
/// a second record): the indexed file fields are collected from whichever record holds them.
fn parse_describe_files(ztag_output: &str) -> Vec<(String, u32, String)> {
    let mut out = Vec::new();
    for record in parse_ztag(ztag_output) {
        let mut i = 0;
        while let Some(depot) = record.get(&format!("depotFile{i}")) {
            let rev = record
                .get(&format!("rev{i}"))
                .and_then(|r| r.trim().parse::<u32>().ok())
                .unwrap_or(0);
            let action = record
                .get(&format!("action{i}"))
                .map(|a| a.trim().to_string())
                .unwrap_or_default();
            out.push((depot.trim().to_string(), rev, action));
            i += 1;
        }
    }
    out
}

/// Parse `p4 -ztag where <depot...>` into a depot-path → client-path map (`//client/...`), used to
/// turn the depot paths from `describe` into workspace repo paths without hardcoding the stream
/// root (the client view does the mapping).
fn parse_where(ztag_output: &str) -> HashMap<String, String> {
    parse_ztag(ztag_output)
        .into_iter()
        .filter_map(|record| {
            let depot = record.get("depotFile")?.trim().to_string();
            let client = record.get("clientFile")?.trim().to_string();
            Some((depot, client))
        })
        .collect()
}

/// Parse `p4 -ztag fstat -Ro //client/...` into the set of *opened* files whose synced revision is
/// behind the head revision (`haveRev < headRev`). `-Ro` restricts the scan to opened files, so
/// this never walks the whole (potentially huge) workspace. Files with no head/have revision (e.g.
/// opened for add) are not out of date.
fn parse_out_of_date(client_name: &str, fstat_output: &str) -> HashSet<RepoPath> {
    parse_ztag(fstat_output)
        .into_iter()
        .filter_map(|record| {
            let have = record.get("haveRev")?.trim().parse::<u64>().ok()?;
            let head = record.get("headRev")?.trim().parse::<u64>().ok()?;
            if have >= head {
                return None;
            }
            client_path_to_repo_path(client_name, record.get("clientFile")?)
        })
        .collect()
}

/// Build the file-history [`CommitData`] for one filelog revision. `parent` is the next-older
/// revision's change number (the linear history parent); B1b integration branching is future work.
fn filelog_rev_to_commit_data(rev: &FilelogRev, parent: Option<u32>) -> CommitData {
    CommitData {
        sha: change_to_oid(rev.change),
        parents: parent.map(change_to_oid).into_iter().collect(),
        author_name: rev.user.clone().into(),
        author_email: SharedString::default(),
        commit_timestamp: rev.time.unwrap_or(0),
        subject: rev.desc.lines().next().unwrap_or_default().to_string().into(),
        message: rev.desc.clone().into(),
    }
}

/// One annotated file line from `p4 -ztag annotate -c -u -i`.
#[derive(Debug, Clone)]
struct AnnotatedLine {
    /// Origin change — annotate's `lower`, i.e. the change that introduced the line. This
    /// matches p4merge's attribution. 0 if unparsable.
    change: u32,
    user: Option<String>,
    /// Unix epoch seconds, parsed from annotate's `time` field.
    time: Option<i64>,
}

/// The `-F` template fed to annotate: one line per record as `lower|user|time`.
///
/// We use `p4 -ztag -F` rather than parsing the default tagged output because the default
/// `... data` field mis-frames records when a depot line lacks a trailing newline (a
/// server-side quirk: consecutive records run together on one physical line, corrupting any
/// newline-based mapping). `-F` makes p4 emit exactly one newline-terminated line per record,
/// which is robust. `lower` is the change that introduced the line (origin attribution,
/// matching p4merge); `|` is a safe separator (never present in a change number or user id).
const ANNOTATE_FORMAT: &str = "%lower%|%user%|%time%";

/// Parse the `-F ANNOTATE_FORMAT` annotate output into one [`AnnotatedLine`] per file line.
///
/// Each line is `<lower>|<user>|<time>`. The leading file-header record has an empty `lower`
/// and is dropped by the `u32` parse.
fn parse_formatted_annotate(output: &str) -> Vec<AnnotatedLine> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '|');
            let change = parts.next()?.trim().parse::<u32>().ok()?;
            let user = parts
                .next()
                .map(str::to_string)
                .filter(|u| !u.is_empty());
            let time = parts.next().and_then(parse_p4_datetime);
            Some(AnnotatedLine {
                change,
                user,
                time,
            })
        })
        .collect()
}

/// Parse a Perforce display timestamp (`YYYY/MM/DD HH:MM:SS`) into epoch seconds.
///
/// Treated as UTC — good enough for the gutter's relative ("3 months ago") rendering. A
/// bare `YYYY/MM/DD` (no time) is accepted too.
fn parse_p4_datetime(s: &str) -> Option<i64> {
    let (date, clock) = s.trim().split_once(' ').unwrap_or((s.trim(), "00:00:00"));
    let mut d = date.split('/');
    let year: i32 = d.next()?.parse().ok()?;
    let month: u8 = d.next()?.parse().ok()?;
    let day: u8 = d.next()?.parse().ok()?;
    let mut t = clock.split(':');
    let hour: u8 = t.next().unwrap_or("0").parse().ok()?;
    let minute: u8 = t.next().unwrap_or("0").parse().ok()?;
    let second: u8 = t.next().unwrap_or("0").parse().ok()?;
    let date = time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()?;
    let clock = time::Time::from_hms(hour, minute, second).ok()?;
    Some(date.with_time(clock).assume_utc().unix_timestamp())
}

/// Parse `p4 -ztag describe -s <changes...>` output into per-change metadata.
///
/// Each change is its own tagged record with `change`, `user`, `time` (epoch seconds), and
/// `desc`. We keep the first line of `desc` as the summary. (`describe` is used rather than
/// `filelog` because, with branch-following annotate, the contributing changes live in the
/// parent stream and `describe` resolves any change number directly.)
fn parse_describe_meta(ztag_output: &str) -> HashMap<u32, ChangeMeta> {
    let mut map: HashMap<u32, ChangeMeta> = HashMap::default();
    for record in parse_ztag(ztag_output) {
        let Some(change) = record.get("change").and_then(|c| c.parse::<u32>().ok()) else {
            continue;
        };
        map.entry(change).or_insert_with(|| ChangeMeta {
            user: record.get("user").cloned(),
            time: record.get("time").and_then(|t| t.parse::<i64>().ok()),
            summary: record
                .get("desc")
                .and_then(|d| d.lines().next())
                .map(|line| line.trim().to_string())
                .filter(|s| !s.is_empty()),
        });
    }
    map
}

/// Combine annotated lines (line -> origin change + author + time, from `-ztag annotate`)
/// with per-change descriptions (from `p4 describe`) into a [`Blame`]. Consecutive lines
/// sharing a change collapse into one [`BlameEntry`].
///
/// Author and time come from annotate itself (every line, no cap); only the description
/// (shown in the hover) comes from `describe`, which is capped — a line whose description
/// wasn't fetched still shows its author, time, and `@change`.
///
/// This is the depot-aligned convenience form (every line attributed), used by the
/// consecutive-grouping unit test. Production blame always routes through
/// [`build_p4_blame_mapped`] (with `None` holes for locally added/edited rows) via the
/// dirty-buffer remap, so this wrapper is test-only.
#[cfg(test)]
fn build_p4_blame(
    lines: &[AnnotatedLine],
    descriptions: &HashMap<u32, ChangeMeta>,
    filename: String,
) -> Blame {
    let mapped: Vec<Option<AnnotatedLine>> = lines.iter().cloned().map(Some).collect();
    build_p4_blame_mapped(&mapped, descriptions, filename)
}

/// Like [`build_p4_blame`] but over a per-*buffer*-line mapping: `Some(line)` carries the
/// depot attribution for that buffer row; `None` means the row is locally added/edited and
/// must show **no** blame (mirroring how git leaves uncommitted rows unattributed).
///
/// Each `None` is simply skipped, leaving that row uncovered by any [`BlameEntry`] range —
/// the editor's `build_blame_entry_sum_tree` fills uncovered rows with `blame: None`, so the
/// gutter and inline indicator render blank there, while attributed rows keep their correct
/// (offset) buffer row via the running index.
fn build_p4_blame_mapped(
    lines: &[Option<AnnotatedLine>],
    descriptions: &HashMap<u32, ChangeMeta>,
    filename: String,
) -> Blame {
    let mut entries = Vec::new();
    let mut messages: HashMap<Oid, String> = HashMap::default();

    let mut i = 0;
    while i < lines.len() {
        // Skip locally added/edited rows: they carry no depot attribution.
        let Some(line) = &lines[i] else {
            i += 1;
            continue;
        };
        let change = line.change;
        let start = i;
        // Group consecutive rows that share the same depot change AND are not holes.
        while i < lines.len()
            && lines[i].as_ref().is_some_and(|l| l.change == change)
        {
            i += 1;
        }
        let oid = change_to_oid(change);
        let summary = descriptions.get(&change).and_then(|m| m.summary.clone());
        if let Some(summary) = &summary {
            messages.entry(oid).or_insert_with(|| summary.clone());
        }
        entries.push(BlameEntry {
            sha: oid,
            range: start as u32..i as u32,
            original_line_number: start as u32 + 1,
            author: line.user.clone(),
            author_mail: None,
            author_time: line.time,
            // `author_offset_date_time()` needs a tz or it falls back to now(); annotate
            // times are parsed as UTC.
            author_tz: line.time.map(|_| "+0000".to_string()),
            committer_name: line.user.clone(),
            committer_email: None,
            committer_time: line.time,
            committer_tz: line.time.map(|_| "+0000".to_string()),
            summary,
            previous: None,
            filename: filename.clone(),
            revision_label: Some(format!("@{change}")),
        });
    }
    Blame { entries, messages }
}

/// Remap a depot annotation onto the local buffer content so each *buffer* line carries the
/// depot attribution of the matching depot line, and locally added/edited lines carry none.
///
/// Why this exists: `p4 annotate` can only annotate a server revision (`#have`), never the
/// dirty buffer — unlike `git blame --contents -`, which blames the exact buffer bytes the
/// editor passes in. So the raw annotation is aligned to the depot file, not the buffer. When
/// the buffer has local edits the rows drift and the wrong author shows. To get git-equivalent
/// behavior we diff the depot `#have` text against the buffer `content` and project the
/// annotation through that diff, exactly as git's `--contents` does internally.
///
/// `depot_text` is the depot `#have` content (`p4 print -q`), expected to have one line per
/// entry in `depot` (which is what [`parse_formatted_annotate`] yields, one record per depot
/// line). The diff is a line-text LCS between `depot_text` and `content`.
///
/// Returns one entry per buffer line (`result.len()` == buffer line count):
/// - `Some(depot_line)` — the buffer line matches a depot line; use that attribution.
/// - `None` — the buffer line was locally inserted or edited; show no blame.
/// Depot lines deleted locally are simply dropped (they have no buffer row).
///
/// Safety net: if `depot_text`'s line count disagrees with `depot.len()` (an annotate/print
/// framing surprise), we cannot trust the alignment, so we return the raw annotation
/// unchanged (`Some` for every depot line) rather than risk mis-attributing — blame is never
/// lost, it just falls back to the previous depot-aligned behavior.
fn remap_annotation_to_content(
    depot: &[AnnotatedLine],
    depot_text: &str,
    content: &Rope,
) -> Vec<Option<AnnotatedLine>> {
    let depot_lines: Vec<&str> = split_lines(depot_text);
    if depot_lines.len() != depot.len() {
        // Alignment we can't trust: fall back to the raw (depot-aligned) annotation.
        return depot.iter().cloned().map(Some).collect();
    }
    let buffer_text = content.to_string();
    let buffer_lines: Vec<&str> = split_lines(&buffer_text);

    // Longest-common-subsequence between depot and buffer line texts. `matches[b] = Some(d)`
    // means buffer line `b` is unchanged and corresponds to depot line `d`.
    let matches = lcs_line_match(&depot_lines, &buffer_lines);
    matches
        .into_iter()
        .map(|d| d.map(|d| depot[d].clone()))
        .collect()
}

/// Split text into logical lines for line-diffing, ignoring a single trailing newline so a
/// file with and without a final newline diff the same. An empty string yields no lines.
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    // `split('\n')` on "a\nb" -> ["a", "b"]; on "" (after stripping a lone "\n") -> [""],
    // but we returned early for the fully-empty case so a single empty line is preserved.
    trimmed.split('\n').map(|l| l.trim_end_matches('\r')).collect()
}

/// Classic LCS over two line-text slices, returning for each `b`-line the matched `a`-index
/// (or `None` if that `b`-line is an insertion/edit). Result length == `b.len()`.
///
/// O(len(a) * len(b)) time and space — fine for source files (annotate is already the slow
/// part). Lines compared by exact text equality, matching git's default whitespace-sensitive
/// blame mapping.
fn lcs_line_match(a: &[&str], b: &[&str]) -> Vec<Option<usize>> {
    let (n, m) = (a.len(), b.len());
    // dp[i][j] = LCS length of a[i..] and b[j..]. (n+1) x (m+1) table.
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    // Backtrack: walk forward choosing matches that preserve the LCS length.
    let mut result = vec![None; m];
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            result[j] = Some(i);
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            // Depot line deleted in buffer: advance depot.
            i += 1;
        } else {
            // Buffer line inserted/edited: leave None, advance buffer.
            j += 1;
        }
    }
    // Any remaining buffer lines (tail insertions) stay None.
    result
}

/// Which Perforce "open" action a save/create/delete should perform.
///
/// Mirrors vscode-perforce's `FileSystemActions`: a save/modify of an existing tracked file
/// opens it for `edit`, a newly-created file opens for `add`, and a deletion opens for
/// `delete`. The verb maps 1:1 onto the `p4` subcommand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum P4OpenAction {
    /// `p4 edit` — make a synced, read-only file writable and open it for edit.
    Edit,
    /// `p4 add` — open a new, not-yet-tracked file for add.
    Add,
    /// `p4 delete` — open a tracked file for delete (also removes it from the workspace).
    Delete,
}

impl P4OpenAction {
    /// The `p4` subcommand verb for this action.
    fn verb(self) -> &'static str {
        match self {
            P4OpenAction::Edit => "edit",
            P4OpenAction::Add => "add",
            P4OpenAction::Delete => "delete",
        }
    }
}

/// Build the argument vector for a `p4 <verb> //client/path...` mutation.
///
/// Pure and side-effect free so the command construction can be unit-tested without
/// shelling out. Verified against vscode-perforce `src/api/commands/basicOps.ts`
/// (`p4 edit/add/delete [-c chnum] files`); we pass no `-c` so files land in the
/// default changelist, and no `-ztag` because the output is not parsed.
fn p4_open_args(action: P4OpenAction, client_name: &str, paths: &[RepoPath]) -> Vec<String> {
    let mut args = Vec::with_capacity(paths.len() + 1);
    args.push(action.verb().to_string());
    for path in paths {
        args.push(format!("//{}/{}", client_name, path.as_unix_str()));
    }
    args
}

/// Build the `p4 move -k <src> <dst>` argument vector (no `-ztag`; output is not parsed).
///
/// `-k` performs the rename on the server **without** moving the workspace file — Zed's own
/// worktree rename already moves the file on disk, so without `-k` p4 would try to move an
/// already-moved file and fail. Source must be open for edit/add first (see [`Self::move_path`]).
/// Verified against vscode-perforce `src/api/commands/basicOps.ts` (`p4 move`) and P4V's
/// `edit`→`move` sequence.
fn p4_move_args(client_name: &str, src: &RepoPath, dst: &RepoPath) -> Vec<String> {
    vec![
        "move".to_string(),
        "-k".to_string(),
        format!("//{}/{}", client_name, src.as_unix_str()),
        format!("//{}/{}", client_name, dst.as_unix_str()),
    ]
}

/// `p4 reopen -c <target> <file>` — move an already-open (pending) file into another changelist.
/// Verified against the P4V drag-between-changelists log (identical command for add/edit files).
fn p4_reopen_args(client_name: &str, target: ChangelistId, file: &RepoPath) -> Vec<String> {
    vec![
        "reopen".to_string(),
        "-c".to_string(),
        target.as_p4_arg(),
        format!("//{}/{}", client_name, file.as_unix_str()),
    ]
}

/// `p4 unshelve -s <source> -c <target> -Af <file>` — restore a shelved file into another
/// changelist as an opened file. This is the P4V behavior when a *shelved* file is dragged onto
/// a changelist (verified against the P4V log). Unlike [`p4_reopen_args`] (which relocates an
/// already-open file), unshelve *copies* the shelf content into the workspace as an open file and
/// leaves the original shelf intact; `-Af` restricts the operation to files (not the stream spec).
fn p4_unshelve_args(
    client_name: &str,
    source: u32,
    target: ChangelistId,
    file: &RepoPath,
) -> Vec<String> {
    vec![
        "unshelve".to_string(),
        "-s".to_string(),
        source.to_string(),
        "-c".to_string(),
        target.as_p4_arg(),
        "-Af".to_string(),
        format!("//{}/{}", client_name, file.as_unix_str()),
    ]
}

/// `p4 revert <file>` — revert one file out of whatever changelist it is open in,
/// restoring the depot `#have` content. p4 does not need the changelist number.
/// Note: reverting a file opened for *add* leaves the local file on disk; the caller
/// decides (with the user) whether to also delete it.
/// Verified against vscode-perforce `src/api/commands/basicOps.ts` (`p4 revert`).
fn p4_revert_args(client_name: &str, file: &RepoPath) -> Vec<String> {
    vec![
        "revert".to_string(),
        format!("//{}/{}", client_name, file.as_unix_str()),
    ]
}

/// `p4 shelve -c <changelist> <file>` — shelve one pending file into its changelist.
/// p4 cannot shelve files that live in the default changelist, so `changelist` is always a
/// numbered one (the panel disables Shelve for default-changelist files).
/// Verified against vscode-perforce `src/api/commands/basicOps.ts` (`p4 shelve`).
fn p4_shelve_args(client_name: &str, changelist: u32, file: &RepoPath) -> Vec<String> {
    vec![
        "shelve".to_string(),
        "-c".to_string(),
        changelist.to_string(),
        format!("//{}/{}", client_name, file.as_unix_str()),
    ]
}

/// Build a Helix Swarm changelist URL `<host>/changes/<chnum>`, mirroring the default
/// behavior of the vscode-perforce `perforce.swarmHost` setting. A trailing slash on the
/// configured host is trimmed so the path is not doubled.
pub fn swarm_changelist_url(host: &str, chnum: u32) -> String {
    format!("{}/changes/{}", host.trim_end_matches('/'), chnum)
}

/// Whether `dir` (the folder Zed opened) lies within `client_root` (the root reported by
/// `p4 info`). The opened folder must be the client root or a subdirectory of it; otherwise the
/// resolved client belongs to a *different* workspace — e.g. an ambient `P4CLIENT` (`p4 set`)
/// leaked because the directory's `.p4config` was not applied — and Perforce integration must be
/// refused rather than silently operate on the wrong client.
///
/// Paths are normalized for Windows: separators unified to `/`, comparison case-insensitive,
/// trailing separators ignored. Containment is anchored on a path-component boundary so a client
/// root `.../ws` does not spuriously contain `.../ws_other`.
fn workspace_root_matches(client_root: &Path, dir: &Path) -> bool {
    fn normalize(p: &Path) -> String {
        p.to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_lowercase()
    }
    let root = normalize(client_root);
    let dir = normalize(dir);
    if root.is_empty() {
        return false;
    }
    dir == root || dir.starts_with(&format!("{root}/"))
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
    /// Max revisions fetched for file history (`p4 filelog -m`); from settings (default 50).
    max_history_count: usize,
    /// Cache of computed blame per file, keyed by the file's on-disk mtime, so we skip
    /// re-running the slow `p4 annotate` when the file hasn't changed (e.g. focus switches,
    /// or the editor's debounced re-blame while a buffer has unsaved edits).
    blame_cache: Arc<Mutex<HashMap<RepoPath, BlameCacheEntry>>>,
    /// File-history commit data (keyed by change number) populated by `initial_graph_data` so the
    /// commit-data reader and `show` can serve rows without re-querying `p4`.
    history_cache: Arc<Mutex<HashMap<u32, CommitData>>>,
    is_trusted: Arc<AtomicBool>,
}

/// Cached *depot-side* blame inputs for a file, keyed by the file's on-disk mtime.
///
/// We cache only the mtime-stable depot data (the per-line annotation and the depot `#have`
/// text), NOT a finished [`Blame`]. The finished blame depends on the (possibly dirty) buffer
/// `content`, which changes on every keystroke while the file's mtime is unchanged — so the
/// dirty-buffer remap ([`remap_annotation_to_content`]) must run fresh on each `blame()` call.
/// The remap is cheap (a line-text LCS); the expensive `p4 annotate`/`describe` round-trips
/// are what this cache skips while the file on disk is unchanged.
struct BlameCacheEntry {
    mtime: Option<SystemTime>,
    /// Per-depot-line annotation (origin change/author/time), aligned 1:1 with `depot_text`.
    lines: Vec<AnnotatedLine>,
    /// Depot `#have` content, used to diff against the buffer for the dirty-buffer remap.
    depot_text: String,
    /// Per-change descriptions (hover summaries), capped to `max_history_count`.
    descriptions: HashMap<u32, ChangeMeta>,
}

impl PerforceRepository {
    /// Construct a backend for an already-detected workspace.
    pub fn new(
        p4_binary_path: PathBuf,
        workspace: PerforceWorkspace,
        envs: Arc<HashMap<String, String>>,
        max_history_count: usize,
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
            max_history_count,
            blame_cache: Arc::new(Mutex::new(HashMap::default())),
            history_cache: Arc::new(Mutex::new(HashMap::default())),
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
        // Validate that the resolved client actually owns the opened folder. `p4 info` resolves the
        // client from `.p4config` / env / `p4 set` (registry) in that precedence; if `.p4config`
        // was not applied (wrong cwd, or none present) an ambient `P4CLIENT` from `p4 set` leaks a
        // *different* workspace whose root does not contain this folder. Enabling it would operate
        // on — and silently auto-checkout files in — the wrong client. Refuse instead.
        let client_root_path = PathBuf::from(&client_root);
        if !workspace_root_matches(&client_root_path, dir) {
            log::error!(
                "perforce: refusing integration — resolved client {client_name:?} has root \
                 {client_root:?}, which does not contain the opened folder {dir:?}. The folder's \
                 `.p4config` was likely not applied and an ambient `P4CLIENT` (`p4 set`) leaked a \
                 different workspace. Perforce integration is disabled for this folder."
            );
            return None;
        }
        log::info!(
            "perforce: detected workspace client={client_name} root={client_root} at {dir:?}"
        );
        Some(PerforceWorkspace {
            client_root: client_root_path,
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
            // 3a — read-only-bit pre-filter: Perforce keeps un-opened files read-only on
            // disk and makes opened (edit/add) files writable. For a scoped refresh we can
            // therefore prove, purely from on-disk permissions, whether *any* candidate
            // could be open. If every candidate is a read-only regular file, none is open,
            // so the `p4 opened` round-trip is guaranteed empty for these paths and we
            // short-circuit to an unchanged status without touching the server. Any
            // writable/missing/directory candidate falls through to the authoritative
            // query below. (Observed: this eliminates ~50 zero-result calls per scan.)
            let candidates: Vec<OpenCandidate> = path_prefixes
                .iter()
                .map(|p| classify_open_candidate(&self.working_directory.join(p.as_std_path())))
                .collect();
            if can_skip_opened_query(&candidates) {
                log::debug!(
                    "perforce: status client={client_name} read-only pre-filter skipped p4 for {n_prefixes} prefix(es)"
                );
                return self.cli.executor.clone().spawn(async move {
                    Ok(GitStatus {
                        entries: Vec::new().into(),
                    })
                });
            }
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

    fn is_perforce(&self) -> bool {
        true
    }

    /// Phase 2 auto-checkout: open files for edit/add/delete (see [`Self::open_for`]).
    fn perforce_open_for(&self, action: P4OpenAction, paths: Vec<RepoPath>) -> Task<Result<()>> {
        self.open_for(action, paths)
    }

    /// Phase 2 auto-checkout: record a file rename as a depot move (see [`Self::move_path`]).
    fn perforce_move(&self, src: RepoPath, dst: RepoPath) -> Task<Result<()>> {
        self.move_path(src, dst)
    }

    /// Changes panel: list pending changelists with their files (see [`Self::changelists`]).
    fn perforce_changelists(&self) -> Task<Result<Vec<PerforceChangelist>>> {
        self.changelists()
    }

    /// Changes panel drag-and-drop: move a file into another changelist (see
    /// [`Self::move_to_changelist`]).
    fn perforce_move_to_changelist(
        &self,
        file: RepoPath,
        target: ChangelistId,
        shelved_source: Option<u32>,
    ) -> Task<Result<()>> {
        self.move_to_changelist(file, target, shelved_source)
    }

    /// Changes panel context menu: revert a single file (see [`Self::revert`]).
    fn perforce_revert(&self, file: RepoPath) -> Task<Result<()>> {
        self.revert(file)
    }

    /// Changes panel context menu: shelve a single file, optionally reverting it afterward
    /// (see [`Self::shelve`]).
    fn perforce_shelve(
        &self,
        changelist: u32,
        file: RepoPath,
        also_revert: bool,
    ) -> Task<Result<()>> {
        self.shelve(changelist, file, also_revert)
    }

    /// Out-of-date badge data: opened files behind head revision (see [`Self::out_of_date_paths`]).
    fn perforce_out_of_date_paths(&self) -> Task<Result<HashSet<RepoPath>>> {
        self.out_of_date_paths()
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

    /// File-history commit metadata (author/time/description) for a change. Serves from the
    /// `initial_graph_data` cache, falling back to `p4 describe -s <change>`.
    fn show(&self, commit: String) -> BoxFuture<'_, Result<CommitDetails>> {
        let cli = self.cli.clone();
        let cache = self.history_cache.clone();
        self.cli
            .executor
            .clone()
            .spawn(async move {
                let change = Oid::from_str(&commit)
                    .ok()
                    .map(|oid| oid_to_change(&oid))
                    .context("invalid Perforce commit id")?;
                if let Some(data) = cache.lock().get(&change).cloned() {
                    return Ok(CommitDetails {
                        sha: commit.into(),
                        message: data.message,
                        commit_timestamp: data.commit_timestamp,
                        author_email: data.author_email,
                        author_name: data.author_name,
                    });
                }
                let change_str = change.to_string();
                let out = cli.run(true, &["describe", "-s", &change_str]).await?;
                let meta = parse_describe_meta(&out)
                    .get(&change)
                    .cloned()
                    .unwrap_or_default();
                Ok(CommitDetails {
                    sha: commit.into(),
                    message: meta.summary.clone().unwrap_or_default().into(),
                    commit_timestamp: meta.time.unwrap_or(0),
                    author_email: SharedString::default(),
                    author_name: meta.user.unwrap_or_default().into(),
                })
            })
            .boxed()
    }

    /// The per-file diff of a change, for the CommitView opened from file history. `describe`
    /// lists the changed files (depot paths + their revs); `where` maps those to workspace repo
    /// paths (without hardcoding the stream root); `print` fetches the new (`#rev`) and old
    /// (`#rev-1`) content for the editor to diff.
    fn load_commit(&self, commit: String, _cx: AsyncApp) -> BoxFuture<'_, Result<CommitDiff>> {
        let cli = self.cli.clone();
        let client_name = self.client_name.clone();
        self.cli.executor.clone().spawn(async move {
            let change = Oid::from_str(&commit)
                .ok()
                .map(|oid| oid_to_change(&oid))
                .context("invalid Perforce commit id")?;
            let change_str = change.to_string();
            let describe_out = cli.run(true, &["describe", "-s", &change_str]).await?;
            let files = parse_describe_files(&describe_out);
            if files.is_empty() {
                return Ok(CommitDiff { files: Vec::new() });
            }

            // One `p4 where` maps every depot path in the change to its client path.
            let mut where_args = vec!["where".to_string()];
            where_args.extend(files.iter().map(|(depot, _, _)| depot.clone()));
            let where_out = cli.run(true, &where_args).await.unwrap_or_default();
            let depot_to_client = parse_where(&where_out);

            let mut commit_files = Vec::new();
            for (depot, rev, action) in files {
                let Some(repo_path) = depot_to_client
                    .get(&depot)
                    .and_then(|client| client_path_to_repo_path(&client_name, client))
                else {
                    continue;
                };
                let is_add = matches!(action.as_str(), "add" | "branch" | "import" | "move/add");
                let is_delete = matches!(action.as_str(), "delete" | "move/delete" | "purge");
                let new_text = if is_delete {
                    None
                } else {
                    cli.run(false, &["print", "-q", &format!("{depot}#{rev}")])
                        .await
                        .ok()
                };
                let old_text = if is_add || rev <= 1 {
                    None
                } else {
                    cli.run(false, &["print", "-q", &format!("{depot}#{}", rev - 1)])
                        .await
                        .ok()
                };
                commit_files.push(CommitFile {
                    path: repo_path,
                    old_text,
                    new_text,
                    is_binary: false,
                });
            }
            Ok(CommitDiff {
                files: commit_files,
            })
        })
        .boxed()
    }

    fn blame(
        &self,
        path: RepoPath,
        content: Rope,
        _line_ending: LineEnding,
    ) -> BoxFuture<'_, Result<Blame>> {
        // Annotate the synced (`#have`) revision — this is what the depot knows; `p4 annotate`
        // can only annotate a server revision, never the dirty buffer (unlike `git blame
        // --contents -`). So the raw annotation is aligned to the depot file. We then diff the
        // depot `#have` text against the editor-supplied `content` (the possibly-dirty buffer)
        // and project the annotation through that diff (see `remap_annotation_to_content`), so
        // locally added/edited lines show no blame and the rest are offset correctly — exactly
        // how git behaves for an unsaved buffer.
        let cli = self.cli.clone();
        let client_path = self.client_syntax_path(&path);
        let abs = self.working_directory.join(path.as_std_path());
        let filename = path.as_unix_str().to_string();
        let max = self.max_history_count;
        let cache = self.blame_cache.clone();
        async move {
            // Skip the slow re-annotate when the file on disk hasn't changed since last time
            // (the editor re-requests blame on a debounce while editing; an unsaved buffer
            // leaves the file mtime untouched). The dirty-buffer remap still runs fresh below
            // against the current `content`, since the cache holds only depot-side inputs.
            let mtime = std::fs::metadata(&abs).and_then(|m| m.modified()).ok();
            if let Some(hit) = cache.lock().get(&path) {
                if hit.mtime == mtime {
                    let mapped =
                        remap_annotation_to_content(&hit.lines, &hit.depot_text, &content);
                    return Ok(build_p4_blame_mapped(
                        &mapped,
                        &hit.descriptions,
                        filename,
                    ));
                }
            }

            let target = format!("{client_path}#have");
            // `-ztag -F "%lower%|%user%|%time%"` makes p4 emit one robust, newline-terminated
            // line per annotated file line (immune to depot lines without trailing newlines).
            // `-i` follows branch history so `lower` is each line's real origin change; `-u`
            // supplies author+time inline (per line, never capped); `-dw` (whitespace-aware
            // diff, as p4merge uses) gives the correct attribution for lines whose blame would
            // otherwise drift to a later, unrelated edit. `-dw` is safe here only because
            // `-F` controls record framing — without `-F` it corrupts the line mapping.
            let (annotate_output, _, _) = cli
                .run_lenient(
                    true,
                    &[
                        "-F",
                        ANNOTATE_FORMAT,
                        "annotate",
                        "-q",
                        "-u",
                        "-c",
                        "-i",
                        "-dw",
                        &target,
                    ],
                )
                .await?;
            let lines = parse_formatted_annotate(&annotate_output);

            // Depot `#have` text, to diff against the buffer for the dirty-buffer remap. This
            // is mtime-stable (depot revision), so it's cached alongside the annotation; the
            // remap itself runs every call against the current buffer `content`.
            let depot_text = cli
                .run(false, &["print", "-q", &target])
                .await
                .unwrap_or_default();

            // Descriptions (hover text only) come from `p4 describe`, capped to
            // `max_history_count` to bound cost. Author/time already came from annotate.
            let mut unique: Vec<u32> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for line in &lines {
                if line.change != 0 && seen.insert(line.change) {
                    unique.push(line.change);
                }
            }
            unique.truncate(max);

            let mut descriptions: HashMap<u32, ChangeMeta> = HashMap::default();
            for chunk in unique.chunks(50) {
                let mut args: Vec<String> = vec!["describe".into(), "-s".into()];
                args.extend(chunk.iter().map(|c| c.to_string()));
                if let Ok(out) = cli.run(true, &args).await {
                    descriptions.extend(parse_describe_meta(&out));
                }
            }
            // Remap the depot annotation onto the (possibly dirty) buffer content so locally
            // added/edited rows show no blame and the rest are offset (git-equivalent).
            let mapped = remap_annotation_to_content(&lines, &depot_text, &content);
            let blame = build_p4_blame_mapped(&mapped, &descriptions, filename);

            cache.lock().insert(
                path,
                BlameCacheEntry {
                    mtime,
                    lines,
                    depot_text,
                    descriptions,
                },
            );
            Ok(blame)
        }
        .boxed()
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

    /// File history (the "View File History" action): stream the file's revisions as graph
    /// commits. `p4 filelog -l -t -m <max>` gives the linear revision list (newest first); each
    /// revision becomes a commit whose parent is the next-older revision (B1b integration branching
    /// is future work). The per-change [`CommitData`] is cached so the commit-data reader and
    /// `show` need no further `p4` calls. Only `LogSource::Path` is supported.
    fn initial_graph_data(
        &self,
        log_source: LogSource,
        _log_order: LogOrder,
        request_tx: async_channel::Sender<Vec<Arc<InitialGraphCommitData>>>,
    ) -> BoxFuture<'_, Result<()>> {
        let LogSource::Path(path) = log_source else {
            return async {
                anyhow::bail!("Perforce only supports per-file history (View File History)")
            }
            .boxed();
        };
        let cli = self.cli.clone();
        let client_path = self.client_syntax_path(&path);
        let max = self.max_history_count;
        let cache = self.history_cache.clone();
        async move {
            let max_arg = max.to_string();
            let args = [
                "filelog",
                "-l",
                "-t",
                "-m",
                max_arg.as_str(),
                client_path.as_str(),
            ];
            let out = cli.run(false, &args).await?;
            let revs = parse_filelog(&out);
            let mut initial = Vec::with_capacity(revs.len());
            {
                let mut cache = cache.lock();
                for (i, rev) in revs.iter().enumerate() {
                    let parent = revs.get(i + 1).map(|r| r.change);
                    let data = filelog_rev_to_commit_data(rev, parent);
                    initial.push(Arc::new(InitialGraphCommitData {
                        sha: data.sha,
                        parents: data.parents.clone(),
                        ref_names: Vec::new(),
                    }));
                    cache.insert(rev.change, data);
                }
            }
            request_tx.send(initial).await.ok();
            Ok(())
        }
        .boxed()
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

    /// Resolve per-commit file-history data on demand. Serves from the `initial_graph_data` cache
    /// (the common case — the rows being displayed), falling back to `p4 describe -s <change>` for
    /// a change not in the cache.
    fn commit_data_reader(&self) -> Result<CommitDataReader> {
        let cli = self.cli.clone();
        let cache = self.history_cache.clone();
        let executor = self.cli.executor.clone();
        Ok(CommitDataReader::from_async_resolver(executor, move |oid| {
            let cli = cli.clone();
            let cache = cache.clone();
            async move {
                let change = oid_to_change(&oid);
                if let Some(data) = cache.lock().get(&change).cloned() {
                    return Ok(data);
                }
                let change_str = change.to_string();
                let out = cli.run(true, &["describe", "-s", &change_str]).await?;
                let meta = parse_describe_meta(&out)
                    .get(&change)
                    .cloned()
                    .unwrap_or_default();
                Ok(CommitData {
                    sha: oid,
                    parents: SmallVec::new(),
                    author_name: meta.user.unwrap_or_default().into(),
                    author_email: SharedString::default(),
                    commit_timestamp: meta.time.unwrap_or(0),
                    subject: meta.summary.clone().unwrap_or_default().into(),
                    message: meta.summary.unwrap_or_default().into(),
                })
            }
        }))
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

/// Phase 2: auto-checkout (open-for-edit/add/delete) operations.
///
/// These are the mutating counterparts to the read-only [`GitRepository::status`] MVP. They
/// mirror vscode-perforce's `FileSystemActions` switches (`editOnFileSave`, `addOnFileCreate`,
/// `deleteOnFileDelete`): as the user edits a Perforce workspace, the affected files are
/// transparently opened in the depot so the local read-only bit is cleared and the change is
/// tracked. Every command runs on the background executor; nothing blocks the main thread.
impl PerforceRepository {
    /// Open the given files for `action` (`p4 edit`/`add`/`delete`).
    ///
    /// Runs `p4 <verb> //client/path...` on the background executor. Files land in the
    /// default changelist (no `-c`), matching vscode-perforce's default behavior. An empty
    /// `paths` is a no-op. Errors surface to the caller (e.g. a save can report that the
    /// `p4 edit` failed) rather than panicking.
    pub fn open_for(&self, action: P4OpenAction, paths: Vec<RepoPath>) -> Task<Result<()>> {
        // Delete needs special handling: `p4 delete` errors on a file already opened for
        // add/edit, so we must revert (and, for edit, then delete) instead.
        if action == P4OpenAction::Delete {
            return self.delete_paths(paths);
        }
        let paths = paths_to_open(action, &self.working_directory, paths);
        if paths.is_empty() {
            return Task::ready(Ok(()));
        }
        let cli = self.cli.clone();
        let args = p4_open_args(action, &self.client_name, &paths);
        let verb = action.verb();
        let n = paths.len();
        self.cli.executor.clone().spawn(async move {
            let (stdout, stderr, ok) = cli.run_lenient(false, &args).await?;
            anyhow::ensure!(ok, "p4 {verb} failed: {stderr}");
            log::info!("perforce: auto-checkout p4 {verb} ({n} path(s)) -> {}", stdout.trim_end());
            Ok(())
        })
    }

    /// Handle a delete, accounting for the file's current open state (a plain `p4 delete`
    /// fails if the file is already open):
    /// - opened for add → `p4 revert` (drop the add; it was never in the depot)
    /// - opened for edit/etc → `p4 revert -k` (un-open, keep the already-removed local file)
    ///   then `p4 delete` (mark for delete in the depot)
    /// - not open → `p4 delete`
    fn delete_paths(&self, paths: Vec<RepoPath>) -> Task<Result<()>> {
        if paths.is_empty() {
            return Task::ready(Ok(()));
        }
        let cli = self.cli.clone();
        let client_name = self.client_name.clone();
        self.cli.executor.clone().spawn(async move {
            let targets: Vec<String> = paths
                .iter()
                .map(|p| format!("//{}/{}", client_name, p.as_unix_str()))
                .collect();

            let mut opened_args = vec!["opened".to_string()];
            opened_args.extend(targets.iter().cloned());
            let (opened_out, _, _) = cli.run_lenient(true, &opened_args).await?;
            let opened = parse_opened_actions(&client_name, &opened_out);

            let (mut revert_add, mut revert_edit, mut plain_delete) =
                (Vec::new(), Vec::new(), Vec::new());
            for (path, target) in paths.iter().zip(targets) {
                match opened.get(path).map(String::as_str) {
                    Some("add" | "move/add" | "branch" | "import") => revert_add.push(target),
                    Some(_) => revert_edit.push(target),
                    None => plain_delete.push(target),
                }
            }

            if !revert_add.is_empty() {
                let mut args = vec!["revert".to_string()];
                args.extend(revert_add);
                cli.run_lenient(false, &args).await.ok();
            }
            if !revert_edit.is_empty() {
                let mut revert = vec!["revert".to_string(), "-k".to_string()];
                revert.extend(revert_edit.iter().cloned());
                cli.run_lenient(false, &revert).await.ok();
                let mut del = vec!["delete".to_string()];
                del.extend(revert_edit);
                cli.run_lenient(false, &del).await.ok();
            }
            if !plain_delete.is_empty() {
                let mut args = vec!["delete".to_string()];
                args.extend(plain_delete);
                cli.run_lenient(false, &args).await.ok();
            }
            log::info!("perforce: auto-delete {} path(s)", paths.len());
            Ok(())
        })
    }

    /// Open-for-edit + move a renamed file in the depot (`p4 edit src` then `p4 move -k src dst`).
    ///
    /// Mirrors vscode-perforce's `moveOneFile` and P4V's `edit`→`move` sequence. The leading
    /// `p4 edit` is best-effort (lenient): `p4 move` requires the source open for edit *or* add,
    /// so a file already open for add is fine and an un-tracked source simply makes the whole
    /// move a no-op. `-k` keeps p4 off the workspace file because Zed's worktree rename performs
    /// the on-disk move itself — this method must run **before** that disk move while `src` still
    /// exists on disk.
    pub fn move_path(&self, src: RepoPath, dst: RepoPath) -> Task<Result<()>> {
        let cli = self.cli.clone();
        let edit_args = p4_open_args(P4OpenAction::Edit, &self.client_name, &[src.clone()]);
        let move_args = p4_move_args(&self.client_name, &src, &dst);
        self.cli.executor.clone().spawn(async move {
            // Open source for edit first; ignore failure (already open for add, or untracked).
            cli.run_lenient(false, &edit_args).await.ok();
            let (stdout, stderr, ok) = cli.run_lenient(false, &move_args).await?;
            anyhow::ensure!(ok, "p4 move failed: {stderr}");
            log::info!(
                "perforce: move {} -> {} -> {}",
                src.as_unix_str(),
                dst.as_unix_str(),
                stdout.trim_end()
            );
            Ok(())
        })
    }

    /// List the client's pending changelists with their open and shelved files, for the Changes
    /// panel.
    ///
    /// Opened files: `p4 -ztag opened` reports every open file with its `change` field in one shot
    /// (avoiding P4V's per-changelist `fstat` N+1), and `p4 -ztag changes -s pending -l -c <client>`
    /// supplies descriptions + surfaces empty pending changelists. `-c <client>` scopes to this
    /// workspace's changelists (a client is owned by one user, so no `-u` filter and no hardcoded
    /// user). Shelved files: fetched with `p4 -ztag fstat -Rs -e <cl>` **only** for changelists
    /// that `changes -l` flagged as having shelves (a global `fstat -Rs` would scan the whole
    /// workspace). Aggregation is the pure [`build_changelists`].
    pub fn changelists(&self) -> Task<Result<Vec<PerforceChangelist>>> {
        let cli = self.cli.clone();
        let client_name = self.client_name.clone();
        let working_directory = self.working_directory.clone();
        let depot_glob = format!("//{client_name}/...");
        let changes_args = vec![
            "changes".to_string(),
            "-s".to_string(),
            "pending".to_string(),
            "-l".to_string(),
            "-c".to_string(),
            client_name.clone(),
        ];
        self.cli.executor.clone().spawn(async move {
            let (opened, _stderr, _ok) = cli.run_lenient(true, &["opened"]).await?;
            let (changes_out, _stderr, _ok) = cli.run_lenient(true, &changes_args).await?;
            let changes = parse_pending_changes(&changes_out);

            // Shelved files: one `fstat -Rs -e <cl>` per changelist that has shelves.
            let mut shelved = Vec::new();
            for change in changes.iter().filter(|c| c.has_shelved) {
                let args = vec![
                    "fstat".to_string(),
                    "-Rs".to_string(),
                    "-e".to_string(),
                    change.number.to_string(),
                    depot_glob.clone(),
                ];
                let (out, _stderr, _ok) = cli.run_lenient(true, &args).await?;
                shelved.extend(parse_shelved_files(&working_directory, &out));
            }

            let groups = build_changelists(&client_name, &opened, &changes, shelved);
            log::debug!(
                "perforce: changelists client={client_name} -> {} group(s)",
                groups.len()
            );
            Ok(groups)
        })
    }

    /// Move a file into `target` changelist via drag-and-drop in the Changes panel.
    ///
    /// `shelved_source` distinguishes the two P4V behaviors (see [`p4_reopen_args`] /
    /// [`p4_unshelve_args`]): `None` = a pending (open) file → `p4 reopen -c` relocates it;
    /// `Some(src)` = a shelved file → `p4 unshelve -s src -c target -Af` restores it into the
    /// target as an open file (the original shelf stays). Best-effort: a non-zero result (e.g.
    /// unshelve needing a manual `p4 resolve`) is surfaced as an error for the caller to log.
    pub fn move_to_changelist(
        &self,
        file: RepoPath,
        target: ChangelistId,
        shelved_source: Option<u32>,
    ) -> Task<Result<()>> {
        let cli = self.cli.clone();
        let args = match shelved_source {
            Some(source) => p4_unshelve_args(&self.client_name, source, target, &file),
            None => p4_reopen_args(&self.client_name, target, &file),
        };
        self.cli.executor.clone().spawn(async move {
            let (stdout, stderr, ok) = cli.run_lenient(false, &args).await?;
            anyhow::ensure!(ok, "p4 move-to-changelist failed: {stderr}");
            log::info!(
                "perforce: move {} -> changelist {} -> {}",
                file.as_unix_str(),
                target.as_p4_arg(),
                stdout.trim_end()
            );
            Ok(())
        })
    }

    /// `p4 revert <file>` — revert one file out of its changelist, restoring depot `#have`.
    /// Reverting a file opened for *add* leaves the local file on disk; the caller (the panel)
    /// decides, with the user, whether to delete it.
    pub fn revert(&self, file: RepoPath) -> Task<Result<()>> {
        let cli = self.cli.clone();
        let args = p4_revert_args(&self.client_name, &file);
        self.cli.executor.clone().spawn(async move {
            let (stdout, stderr, ok) = cli.run_lenient(false, &args).await?;
            anyhow::ensure!(ok, "p4 revert failed: {stderr}");
            log::info!("perforce: revert {} -> {}", file.as_unix_str(), stdout.trim_end());
            Ok(())
        })
    }

    /// `p4 shelve -c <changelist> <file>`, then (for "Shelve and Revert") `p4 revert <file>`.
    /// The revert runs only after a successful shelve, so the shelf preserves the work before the
    /// workspace copy is restored to depot `#have`. `changelist` must be numbered (p4 cannot
    /// shelve from the default changelist).
    pub fn shelve(&self, changelist: u32, file: RepoPath, also_revert: bool) -> Task<Result<()>> {
        let cli = self.cli.clone();
        let shelve_args = p4_shelve_args(&self.client_name, changelist, &file);
        let revert_args = p4_revert_args(&self.client_name, &file);
        self.cli.executor.clone().spawn(async move {
            let (stdout, stderr, ok) = cli.run_lenient(false, &shelve_args).await?;
            anyhow::ensure!(ok, "p4 shelve failed: {stderr}");
            log::info!(
                "perforce: shelve {} -> changelist {} -> {}",
                file.as_unix_str(),
                changelist,
                stdout.trim_end()
            );
            if also_revert {
                let (rstdout, rstderr, rok) = cli.run_lenient(false, &revert_args).await?;
                anyhow::ensure!(rok, "p4 revert (after shelve) failed: {rstderr}");
                log::info!(
                    "perforce: revert-after-shelve {} -> {}",
                    file.as_unix_str(),
                    rstdout.trim_end()
                );
            }
            Ok(())
        })
    }

    /// The set of *opened* workspace files whose synced revision is behind the head revision
    /// (`p4 fstat -Ro`). `-Ro` scans only opened files, so this never walks the whole workspace —
    /// safe for very large clients. Used to badge out-of-date files in the Changes and project
    /// panels.
    pub fn out_of_date_paths(&self) -> Task<Result<HashSet<RepoPath>>> {
        let cli = self.cli.clone();
        let client_name = self.client_name.clone();
        let glob = format!("//{client_name}/...");
        self.cli.executor.clone().spawn(async move {
            let (stdout, _stderr, ok) = cli.run_lenient(true, &["fstat", "-Ro", &glob]).await?;
            if !ok {
                // No opened files (or transient error) — nothing to flag.
                return Ok(HashSet::default());
            }
            Ok(parse_out_of_date(&client_name, &stdout))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::FileStatus;

    #[test]
    fn workspace_root_match_accepts_dir_inside_client_root() {
        // The opened folder is the client root itself, or a subdirectory of it.
        assert!(workspace_root_matches(
            Path::new("E:/Projects/ws_branch_a"),
            Path::new("E:/Projects/ws_branch_a"),
        ));
        assert!(workspace_root_matches(
            Path::new("E:/Projects/ws_branch_a"),
            Path::new("E:/Projects/ws_branch_a/sub/dir"),
        ));
    }

    #[test]
    fn workspace_root_match_normalizes_slashes_and_case() {
        // p4 reports clientRoot with mixed separators (`E:/Projects\ws`); Windows is case
        // insensitive. Both must still match the opened path.
        assert!(workspace_root_matches(
            Path::new("E:/Projects\\ws_branch_a"),
            Path::new("e:\\projects\\WS_BRANCH_A"),
        ));
    }

    #[test]
    fn workspace_root_match_rejects_mismatched_client() {
        // The bug: opened folder is branch_b but p4 resolved a branch_a client (e.g. an ambient
        // `p4 set P4CLIENT` leaked because `.p4config` was not applied). branch_b is NOT inside the
        // branch_a client root, so the workspace must be rejected (no Perforce integration).
        assert!(!workspace_root_matches(
            Path::new("E:/Projects/ws_branch_a"),
            Path::new("E:/Projects/ws_branch_b"),
        ));
        // A shared prefix that is not a path-component boundary must not count as containment.
        assert!(!workspace_root_matches(
            Path::new("E:/Projects/ws"),
            Path::new("E:/Projects/ws_other"),
        ));
    }

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
    fn pending_changes_keep_multiline_desc_with_blank_lines() {
        // Real `changes -l` framing: desc is the last field, its value spans multiple lines
        // *including a blank line*, and records are separated by blank line(s). The blank line
        // inside the description must NOT split the record or truncate the description.
        let output = "\
... change 7398454
... status pending
... desc single line summary


... change 7397357
... status pending
... shelved
... desc first line

second paragraph
- bullet a
- bullet b


... change 7334528
... status pending
... desc only line
";
        let parsed = parse_pending_changes(output);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].number, 7398454);
        assert_eq!(parsed[0].description, "single line summary");
        assert!(!parsed[0].has_shelved);
        assert_eq!(parsed[1].number, 7397357);
        assert_eq!(
            parsed[1].description,
            "first line\n\nsecond paragraph\n- bullet a\n- bullet b"
        );
        // `... shelved` flag detected (must not be swallowed by the multi-line desc continuation).
        assert!(parsed[1].has_shelved);
        assert_eq!(parsed[2].number, 7334528);
        assert_eq!(parsed[2].description, "only line");
        assert!(!parsed[2].has_shelved);
    }

    #[test]
    fn pending_changes_handle_desc_not_last_field() {
        // `describe -s` framing puts fields after desc; the continuation must stop at the next
        // `... <field>` line, not bleed the following fields into the description.
        let output = "\
... change 42
... user someuser
... desc line one

line two
... status pending
... changeType public
";
        let parsed = parse_pending_changes(output);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].number, 42);
        assert_eq!(parsed[0].description, "line one\n\nline two");
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

    // Real `p4 -ztag changes -s pending -l` output shape: one record per pending changelist.
    // Descriptions are arbitrary fixture text. `6500000` has no opened files (an empty pending
    // changelist) and must still appear in the grouping.
    const CHANGES_FIXTURE: &str = "\
... change 6596347
... time 1700000000
... user someuser
... client some_client_name
... status pending
... changeType public
... desc First feature work

... change 6500000
... time 1699000000
... user someuser
... client some_client_name
... status pending
... changeType public
... desc Older change
";

    #[test]
    fn changelists_group_files_and_sort_default_first_then_descending() {
        let changes = parse_pending_changes(CHANGES_FIXTURE);
        let groups = build_changelists("some_client_name", OPENED_FIXTURE, &changes, Vec::new());

        // Default first, then numbered changelists in descending order. The empty pending
        // changelist (6500000) still appears.
        let ids: Vec<ChangelistId> = groups.iter().map(|g| g.id).collect();
        assert_eq!(
            ids,
            vec![
                ChangelistId::Default,
                ChangelistId::Numbered(6596347),
                ChangelistId::Numbered(6500000),
            ]
        );

        // Default changelist: the one edited file, no fabricated description.
        let default = &groups[0];
        assert_eq!(default.description, "");
        assert_eq!(default.files.len(), 1);
        assert_eq!(default.files[0].path.as_unix_str(), "b/edited.cpp");
        assert!(default.files[0].status.is_modified());
        assert!(!default.files[0].shelved);

        // Numbered changelist: description from `changes`, files sorted by path.
        let cl = &groups[1];
        assert_eq!(cl.description, "First feature work");
        let files: Vec<(&str, bool)> = cl
            .files
            .iter()
            .map(|f| (f.path.as_unix_str(), f.status.is_deleted()))
            .collect();
        assert_eq!(files, vec![("a/added.md", false), ("c/gone.txt", true)]);

        // Empty pending changelist: present, described, no files.
        let empty = &groups[2];
        assert_eq!(empty.description, "Older change");
        assert!(empty.files.is_empty());
    }

    #[test]
    fn local_path_maps_under_client_root() {
        // `fstat` reports `clientFile` as a local OS path (mixed separators on Windows); it must
        // map to a repo-relative path by stripping the client root, case-insensitively.
        let root = Path::new("E:/Projects/some_client_name");
        let p = local_path_to_repo_path(root, "E:/Projects\\some_client_name\\a\\shelved.md")
            .expect("should map under root");
        assert_eq!(p.as_unix_str(), "a/shelved.md");
        // Different case in the drive/root still maps (Windows paths are case-insensitive).
        let p2 = local_path_to_repo_path(root, "e:/projects/SOME_CLIENT_NAME/b/x.txt")
            .expect("case-insensitive root");
        assert_eq!(p2.as_unix_str(), "b/x.txt");
        // A path outside the client root does not map.
        assert!(local_path_to_repo_path(root, "D:/elsewhere/a.md").is_none());
    }

    // Real `p4 -ztag fstat -Rs -e <cl>` shape: one record per shelved file (local `clientFile`),
    // then a trailing changelist-description record with no `clientFile` that must be skipped.
    const SHELVED_FIXTURE: &str = "\
... depotFile //Depot.Project/Release/branch_x/a/shelved_add.md
... clientFile E:/Projects\\some_client_name\\a\\shelved_add.md
... shelved
... isMapped
... action add
... change 6596347
... type text

... desc a shelved changelist
";

    #[test]
    fn shelved_files_parse_with_local_paths() {
        let root = Path::new("E:/Projects/some_client_name");
        let parsed = parse_shelved_files(root, SHELVED_FIXTURE);
        assert_eq!(parsed.len(), 1);
        let (change, file) = &parsed[0];
        assert_eq!(*change, 6596347);
        assert_eq!(file.path.as_unix_str(), "a/shelved_add.md");
        assert!(file.shelved);
        assert!(file.status.is_created());
    }

    #[test]
    fn changelists_append_shelved_after_opened() {
        let changes = parse_pending_changes(CHANGES_FIXTURE);
        let shelved = vec![(
            6596347u32,
            ChangelistFile {
                path: repo_path("a/shelved.md"),
                status: action_to_status("edit").unwrap(),
                shelved: true,
            },
        )];
        let groups = build_changelists("some_client_name", OPENED_FIXTURE, &changes, shelved);

        // 6596347 had two opened files (add + delete); the shelved file is appended after them.
        let cl = &groups[1];
        assert_eq!(cl.id, ChangelistId::Numbered(6596347));
        assert_eq!(cl.files.len(), 3);
        assert_eq!(cl.files.iter().filter(|f| !f.shelved).count(), 2);
        let last = cl.files.last().unwrap();
        assert!(last.shelved);
        assert_eq!(last.path.as_unix_str(), "a/shelved.md");
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

    // ---- 3a: read-only-bit pre-filter decision logic ----

    #[test]
    fn skip_decision_all_readonly_skips() {
        // Every candidate provably not open ⇒ safe to skip the server.
        let candidates = vec![
            OpenCandidate::DefinitelyNotOpen,
            OpenCandidate::DefinitelyNotOpen,
        ];
        assert!(can_skip_opened_query(&candidates));
    }

    #[test]
    fn skip_decision_any_maybe_open_queries() {
        // A single writable/missing/dir candidate forces a server query.
        let candidates = vec![
            OpenCandidate::DefinitelyNotOpen,
            OpenCandidate::MaybeOpen,
            OpenCandidate::DefinitelyNotOpen,
        ];
        assert!(!can_skip_opened_query(&candidates));
    }

    #[test]
    fn skip_decision_empty_never_skips() {
        // Empty == full/large refresh; must never be routed to a skip.
        assert!(!can_skip_opened_query(&[]));
    }

    #[test]
    fn classify_readonly_file_is_not_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro.txt");
        std::fs::write(&path, b"depot content").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();
        assert_eq!(
            classify_open_candidate(&path),
            OpenCandidate::DefinitelyNotOpen
        );
    }

    #[test]
    fn classify_writable_file_maybe_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rw.txt");
        std::fs::write(&path, b"opened for edit").unwrap();
        // Freshly written file is writable by default on both Windows and Unix.
        assert_eq!(classify_open_candidate(&path), OpenCandidate::MaybeOpen);
    }

    #[test]
    fn classify_missing_path_maybe_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does_not_exist.txt");
        // Missing locally could mean open-for-delete: must not be ruled out.
        assert_eq!(classify_open_candidate(&path), OpenCandidate::MaybeOpen);
    }

    #[test]
    fn classify_directory_maybe_open() {
        let dir = tempfile::tempdir().unwrap();
        // A directory prefix's children are not inspected here: must query.
        assert_eq!(classify_open_candidate(dir.path()), OpenCandidate::MaybeOpen);
    }

    // ---- Phase 2: auto-checkout p4 command construction ----

    fn repo_path(unix: &str) -> RepoPath {
        RepoPath::from_rel_path(RelPath::unix(unix).unwrap())
    }

    #[test]
    fn open_action_verbs() {
        assert_eq!(P4OpenAction::Edit.verb(), "edit");
        assert_eq!(P4OpenAction::Add.verb(), "add");
        assert_eq!(P4OpenAction::Delete.verb(), "delete");
    }

    #[test]
    fn open_args_build_client_syntax_paths() {
        let args = p4_open_args(
            P4OpenAction::Edit,
            "some_client_name",
            &[repo_path("a/added.md"), repo_path("b/edited.cpp")],
        );
        assert_eq!(
            args,
            vec![
                "edit".to_string(),
                "//some_client_name/a/added.md".to_string(),
                "//some_client_name/b/edited.cpp".to_string(),
            ]
        );
    }

    #[test]
    fn open_args_add_and_delete_use_correct_verb() {
        let add = p4_open_args(P4OpenAction::Add, "c", &[repo_path("new.txt")]);
        assert_eq!(add, vec!["add".to_string(), "//c/new.txt".to_string()]);
        let del = p4_open_args(P4OpenAction::Delete, "c", &[repo_path("gone.txt")]);
        assert_eq!(del, vec!["delete".to_string(), "//c/gone.txt".to_string()]);
    }

    #[test]
    fn open_args_no_ztag_flag_present() {
        // Mutations must NOT pass -ztag: only the verb and paths.
        let args = p4_open_args(P4OpenAction::Edit, "c", &[repo_path("f.rs")]);
        assert!(!args.iter().any(|a| a == "-ztag"));
        assert_eq!(args[0], "edit");
    }

    #[test]
    fn move_args_keep_workspace_client_syntax() {
        // `p4 move -k` records the depot rename only; Zed performs the on-disk move, so `-k`
        // must always be present (otherwise p4 would fight Zed over moving the file).
        let args = p4_move_args(
            "some_client_name",
            &repo_path("a/old.txt"),
            &repo_path("b/new.txt"),
        );
        assert_eq!(
            args,
            vec![
                "move".to_string(),
                "-k".to_string(),
                "//some_client_name/a/old.txt".to_string(),
                "//some_client_name/b/new.txt".to_string(),
            ]
        );
    }

    #[test]
    fn reopen_args_target_changelist() {
        // Numbered target.
        let args = p4_reopen_args("c", ChangelistId::Numbered(60000001), &repo_path("a/b.txt"));
        assert_eq!(
            args,
            vec![
                "reopen".to_string(),
                "-c".to_string(),
                "60000001".to_string(),
                "//c/a/b.txt".to_string(),
            ]
        );
        // Default target uses the literal "default".
        let default = p4_reopen_args("c", ChangelistId::Default, &repo_path("a/b.txt"));
        assert_eq!(default[2], "default");
    }

    #[test]
    fn unshelve_args_restore_into_target() {
        // Shelved-file drag: `unshelve -s <source> -c <target> -Af <file>` (files only).
        let args = p4_unshelve_args(
            "c",
            7369367,
            ChangelistId::Numbered(7397357),
            &repo_path("a/b.md"),
        );
        assert_eq!(
            args,
            vec![
                "unshelve".to_string(),
                "-s".to_string(),
                "7369367".to_string(),
                "-c".to_string(),
                "7397357".to_string(),
                "-Af".to_string(),
                "//c/a/b.md".to_string(),
            ]
        );
    }

    #[test]
    fn out_of_date_flags_only_stale_opened_files() {
        // `p4 -ztag fstat -Ro //client/...` reports opened files with haveRev/headRev. A file is
        // out of date when haveRev < headRev. Added files (no revs) and up-to-date files are not.
        let ztag = "\
... depotFile //Depot.Project/Release/branch_x/a/stale.cpp
... clientFile //some_client_name/a/stale.cpp
... headRev 5
... haveRev 3
... action edit

... depotFile //Depot.Project/Release/branch_x/b/current.cpp
... clientFile //some_client_name/b/current.cpp
... headRev 7
... haveRev 7
... action edit

... depotFile //Depot.Project/Release/branch_x/c/added.md
... clientFile //some_client_name/c/added.md
... action add
";
        let stale = parse_out_of_date("some_client_name", ztag);
        assert_eq!(stale.len(), 1);
        assert!(stale.contains(&repo_path("a/stale.cpp")));
        assert!(!stale.contains(&repo_path("b/current.cpp")));
        assert!(!stale.contains(&repo_path("c/added.md")));
    }

    #[test]
    fn describe_files_parse_indexed_records() {
        // `p4 -ztag describe -s <change>` lists changed files as indexed fields depotFile0/rev0/
        // action0, depotFile1/... (synthetic fixture).
        let ztag = "\
... change 6596347
... user someuser
... time 1700000000
... depotFile0 //Depot.Project/Release/branch_x/a/b.cpp
... action0 edit
... type0 text
... rev0 3
... depotFile1 //Depot.Project/Release/branch_x/c/new.md
... action1 add
... type1 text
... rev1 1
";
        let files = parse_describe_files(ztag);
        assert_eq!(
            files,
            vec![
                ("//Depot.Project/Release/branch_x/a/b.cpp".to_string(), 3, "edit".to_string()),
                ("//Depot.Project/Release/branch_x/c/new.md".to_string(), 1, "add".to_string()),
            ]
        );
    }

    #[test]
    fn where_maps_depot_to_client() {
        // `p4 -ztag where <depot...>` maps depot paths to client paths (synthetic fixture).
        let ztag = "\
... depotFile //Depot.Project/Release/branch_x/a/b.cpp
... clientFile //some_client_name/a/b.cpp
... path E:\\Projects\\some_client_name\\a\\b.cpp

... depotFile //Depot.Project/Release/branch_x/c/new.md
... clientFile //some_client_name/c/new.md
... path E:\\Projects\\some_client_name\\c\\new.md
";
        let map = parse_where(ztag);
        assert_eq!(
            map.get("//Depot.Project/Release/branch_x/a/b.cpp").map(String::as_str),
            Some("//some_client_name/a/b.cpp")
        );
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn oid_change_roundtrip() {
        // A change number encodes into the leading bytes of a synthetic Oid and back.
        let oid = change_to_oid(6596347);
        assert_eq!(oid_to_change(&oid), 6596347);
        assert_eq!(oid_to_change(&change_to_oid(1)), 1);
    }

    #[test]
    fn filelog_parses_revisions_newest_first() {
        // `p4 filelog -l -t <path>` text output: a depot-path header, then one `... #<rev>` record
        // per revision with a tab-indented description. (Identifiers are synthetic fixtures.)
        let output = "\
//Depot.Project/Release/branch_x/a/b.cpp
... #3 change 6596347 edit on 2023/11/14 18:30:00 by someuser@some_client_name (text)

\tFix the thing
\tsecond line

... #2 change 6500000 add on 2023/06/01 09:00:00 by someuser@some_client_name (text)

\tInitial work
";
        let revs = parse_filelog(output);
        assert_eq!(revs.len(), 2);
        assert_eq!(revs[0].rev, 3);
        assert_eq!(revs[0].change, 6596347);
        assert_eq!(revs[0].action, "edit");
        assert_eq!(revs[0].user, "someuser");
        assert_eq!(revs[0].desc, "Fix the thing\nsecond line");
        assert!(revs[0].time.is_some());
        assert_eq!(revs[1].rev, 2);
        assert_eq!(revs[1].change, 6500000);
        assert_eq!(revs[1].action, "add");
        assert_eq!(revs[1].desc, "Initial work");
    }

    #[test]
    fn filelog_ignores_integration_lines() {
        // Integration records (`... ... copy/edit from //depot#n`) must not be parsed as revisions.
        let output = "\
//Depot.Project/Release/branch_x/a/b.cpp
... #1 change 6400000 branch on 2023/01/01 00:00:00 by someuser@some_client_name (text)
... ... branch from //Depot.Project/Main/a/b.cpp#4

\tBranched in
";
        let revs = parse_filelog(output);
        assert_eq!(revs.len(), 1);
        assert_eq!(revs[0].change, 6400000);
        assert_eq!(revs[0].desc, "Branched in");
    }

    #[test]
    fn revert_args_just_the_file() {
        // Per-file revert from the context menu: `p4 revert <file>`. p4 reverts it out of
        // whatever changelist it is open in, so no `-c` is needed.
        let args = p4_revert_args("some_client_name", &repo_path("a/b.cpp"));
        assert_eq!(
            args,
            vec![
                "revert".to_string(),
                "//some_client_name/a/b.cpp".to_string(),
            ]
        );
    }

    #[test]
    fn shelve_args_into_numbered_changelist() {
        // `p4 shelve -c <cl> <file>` — shelve one pending file into its (numbered) changelist.
        // p4 cannot shelve from the default changelist, so the target is always numbered.
        let args = p4_shelve_args("c", 6596347, &repo_path("a/b.cpp"));
        assert_eq!(
            args,
            vec![
                "shelve".to_string(),
                "-c".to_string(),
                "6596347".to_string(),
                "//c/a/b.cpp".to_string(),
            ]
        );
    }

    #[test]
    fn swarm_url_appends_changes_path() {
        // `<host>/changes/<chnum>`, mirroring vscode-perforce's default swarmHost handling.
        assert_eq!(
            swarm_changelist_url("https://swarm.example.com", 6596347),
            "https://swarm.example.com/changes/6596347"
        );
        // A trailing slash on the host must not produce a double slash.
        assert_eq!(
            swarm_changelist_url("https://swarm.example.com/", 6596347),
            "https://swarm.example.com/changes/6596347"
        );
    }

    #[test]
    fn opened_actions_parse() {
        let ztag = "\
... clientFile //some_client_name/a/edited.cpp
... action edit

... clientFile //some_client_name/b/added.md
... action add
";
        let map = parse_opened_actions("some_client_name", ztag);
        assert_eq!(map.get(&repo_path("a/edited.cpp")).map(String::as_str), Some("edit"));
        assert_eq!(map.get(&repo_path("b/added.md")).map(String::as_str), Some("add"));
    }

    #[test]
    fn edit_skips_already_writable_files() {
        // An already-writable file is already open for edit; a second `p4 edit` is wasted.
        let dir = tempfile::tempdir().unwrap();
        let ro_path = dir.path().join("ro.txt");
        std::fs::write(&ro_path, b"synced").unwrap();
        let mut perms = std::fs::metadata(&ro_path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&ro_path, perms).unwrap();
        std::fs::write(dir.path().join("rw.txt"), b"already open").unwrap();

        // Edit: only the read-only file needs checkout.
        let kept = paths_to_open(
            P4OpenAction::Edit,
            dir.path(),
            vec![repo_path("ro.txt"), repo_path("rw.txt")],
        );
        assert_eq!(kept, vec![repo_path("ro.txt")]);

        // Add/Delete are not gated by the read-only bit.
        let added = paths_to_open(
            P4OpenAction::Add,
            dir.path(),
            vec![repo_path("ro.txt"), repo_path("rw.txt")],
        );
        assert_eq!(added.len(), 2);
    }

    // ---- blame (p4 annotate + filelog) ----

    #[test]
    fn formatted_annotate_parses_origin_change_user_time() {
        // `-F "%lower%|%user%|%time%"` output: one `lower|user|time` line per file line. The
        // header record has an empty lower and is dropped.
        let out = "\
||
1001|devuser1|2020/01/02 03:04:05
1002|devuser2|2020/02/03 04:05:06
";
        let lines = parse_formatted_annotate(out);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].change, 1001);
        assert_eq!(lines[0].user.as_deref(), Some("devuser1"));
        assert!(lines[0].time.is_some()); // real timestamp, not None -> not "just now"
        assert_eq!(lines[1].change, 1002);
        assert_eq!(lines[1].user.as_deref(), Some("devuser2"));
    }

    #[test]
    fn p4_datetime_parses() {
        let t1 = parse_p4_datetime("2020/01/02 16:53:00").unwrap();
        let t0 = parse_p4_datetime("2020/01/02 00:00:00").unwrap();
        assert_eq!(t1 - t0, 16 * 3600 + 53 * 60); // time-of-day delta
        assert!(parse_p4_datetime("2020/01/02").is_some());
        assert!(parse_p4_datetime("garbage").is_none());
    }

    #[test]
    fn describe_meta_parses_change_records() {
        // `p4 -ztag describe -s` shape: one blank-line-separated record per change.
        let ztag = "\
... change 2001
... user devuser1
... time 1700000000
... desc add curves

... change 2002
... user devuser2
... time 1690000000
... desc [CODE] initial import
";
        let meta = parse_describe_meta(ztag);
        let a = meta.get(&2001).unwrap();
        assert_eq!(a.user.as_deref(), Some("devuser1"));
        assert_eq!(a.time, Some(1700000000));
        assert_eq!(a.summary.as_deref(), Some("add curves"));
        assert_eq!(meta.get(&2002).unwrap().summary.as_deref(), Some("[CODE] initial import"));
    }

    #[test]
    fn build_blame_groups_consecutive_changes() {
        let lines = vec![
            AnnotatedLine {
                change: 2001,
                user: Some("devuser1".into()),
                time: Some(1700000000),
            },
            AnnotatedLine {
                change: 2001,
                user: Some("devuser1".into()),
                time: Some(1700000000),
            },
            AnnotatedLine {
                change: 2002,
                user: Some("devuser2".into()),
                time: Some(1690000000),
            },
        ];
        let mut descriptions = HashMap::default();
        descriptions.insert(
            2001,
            ChangeMeta {
                user: None,
                time: None,
                summary: Some("add curves".into()),
            },
        );
        let blame = build_p4_blame(&lines, &descriptions, "a/b.cpp".into());
        assert_eq!(blame.entries.len(), 2);
        assert_eq!(blame.entries[0].range, 0..2);
        assert_eq!(blame.entries[0].revision_label.as_deref(), Some("@2001"));
        assert_eq!(blame.entries[0].author.as_deref(), Some("devuser1"));
        assert_eq!(blame.entries[0].author_time, Some(1700000000));
        assert_eq!(blame.entries[0].summary.as_deref(), Some("add curves"));
        assert_eq!(blame.entries[1].range, 2..3);
        assert_eq!(blame.entries[1].revision_label.as_deref(), Some("@2002"));
        assert_eq!(blame.entries[1].author.as_deref(), Some("devuser2"));
        // A line whose description wasn't fetched still has author/time, just no summary.
        assert_eq!(blame.entries[1].summary, None);
        // Distinct synthetic oids per change (drives the gutter color).
        assert_ne!(blame.entries[0].sha, blame.entries[1].sha);
        assert_eq!(
            blame.messages.get(&change_to_oid(2001)).map(String::as_str),
            Some("add curves")
        );
    }

    // ---- dirty-buffer remap: align depot annotation onto the local buffer ----

    fn annotated(change: u32) -> AnnotatedLine {
        AnnotatedLine {
            change,
            user: Some(format!("user{change}")),
            time: Some(1_700_000_000 + change as i64),
        }
    }

    #[test]
    fn remap_clean_buffer_is_identity() {
        // Buffer identical to depot#have: every line keeps its depot attribution, in order.
        let depot = vec![annotated(1001), annotated(1001), annotated(1002)];
        let depot_text = "a\nb\nc\n";
        let content = Rope::from("a\nb\nc\n");
        let mapped = remap_annotation_to_content(&depot, depot_text, &content);
        let changes: Vec<Option<u32>> = mapped.iter().map(|m| m.as_ref().map(|l| l.change)).collect();
        assert_eq!(changes, vec![Some(1001), Some(1001), Some(1002)]);
    }

    #[test]
    fn remap_inserted_line_gets_no_blame_and_shifts_rest() {
        // Depot has 3 lines (a,b,c). Buffer inserts a new line "x" between b and c.
        // The inserted line must carry NO depot attribution (None); a/b/c keep theirs,
        // and c is correctly offset down by one row.
        let depot = vec![annotated(1001), annotated(1002), annotated(1003)];
        let depot_text = "a\nb\nc\n";
        let content = Rope::from("a\nb\nx\nc\n");
        let mapped = remap_annotation_to_content(&depot, depot_text, &content);
        let changes: Vec<Option<u32>> = mapped.iter().map(|m| m.as_ref().map(|l| l.change)).collect();
        assert_eq!(changes, vec![Some(1001), Some(1002), None, Some(1003)]);
    }

    #[test]
    fn remap_edited_line_gets_no_blame() {
        // Depot a,b,c. Buffer edits the middle line b -> B. The edited line shows no blame;
        // a and c retain their original depot authors at the right rows.
        let depot = vec![annotated(1001), annotated(1002), annotated(1003)];
        let depot_text = "a\nb\nc\n";
        let content = Rope::from("a\nB\nc\n");
        let mapped = remap_annotation_to_content(&depot, depot_text, &content);
        let changes: Vec<Option<u32>> = mapped.iter().map(|m| m.as_ref().map(|l| l.change)).collect();
        assert_eq!(changes, vec![Some(1001), None, Some(1003)]);
    }

    #[test]
    fn remap_deleted_line_drops_depot_row() {
        // Depot a,b,c. Buffer deletes b. Remaining buffer lines a,c keep their depot authors.
        let depot = vec![annotated(1001), annotated(1002), annotated(1003)];
        let depot_text = "a\nb\nc\n";
        let content = Rope::from("a\nc\n");
        let mapped = remap_annotation_to_content(&depot, depot_text, &content);
        let changes: Vec<Option<u32>> = mapped.iter().map(|m| m.as_ref().map(|l| l.change)).collect();
        assert_eq!(changes, vec![Some(1001), Some(1003)]);
    }

    #[test]
    fn remap_mismatched_counts_falls_back_to_raw() {
        // If depot_text line count disagrees with the annotation length, keep the raw
        // depot-aligned annotation (never lose blame).
        let depot = vec![annotated(1001), annotated(1002)];
        let depot_text = "a\nb\nc\n"; // 3 lines vs 2 annotation entries
        let content = Rope::from("a\nb\nc\n");
        let mapped = remap_annotation_to_content(&depot, depot_text, &content);
        let changes: Vec<Option<u32>> = mapped.iter().map(|m| m.as_ref().map(|l| l.change)).collect();
        assert_eq!(changes, vec![Some(1001), Some(1002)]);
    }

    #[test]
    fn blame_from_remapped_hides_holes_and_offsets() {
        // End-to-end through build_p4_blame_mapped: an inserted line (None) produces a gap in
        // the blame ranges (so the editor renders no gutter/inline for it), and the depot
        // entry below the insertion is shifted to its new buffer row.
        let mapped = vec![
            Some(annotated(2001)),
            None, // locally inserted/edited -> no blame
            Some(annotated(2002)),
        ];
        let descriptions = HashMap::default();
        let blame = build_p4_blame_mapped(&mapped, &descriptions, "a/b.cpp".into());
        // Two attributed entries; the hole at row 1 is left uncovered (rendered blank).
        assert_eq!(blame.entries.len(), 2);
        assert_eq!(blame.entries[0].range, 0..1);
        assert_eq!(blame.entries[0].revision_label.as_deref(), Some("@2001"));
        assert_eq!(blame.entries[1].range, 2..3);
        assert_eq!(blame.entries[1].revision_label.as_deref(), Some("@2002"));
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
