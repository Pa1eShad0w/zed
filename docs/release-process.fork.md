# Release Process (Fork)

Runbook for cutting a fork release. Use this every time you want a new `vX.Y.Z-fork.N` tag and the corresponding GitHub Release / intranet mirror.

## Prereqs

- `cargo` + `git` on `PATH`
- `cargo-edit` recommended: `cargo install cargo-edit`. Without it, the bump script falls back to a regex rewrite of `crates/zed/Cargo.toml` + `Cargo.lock` — works but less robust.
- Push permission on the release branch (`perforce-integration` or, during the in-progress fork-update-system migration, also `fork-update-system`).
- `$GIT_EDITOR` or `$EDITOR` set. PowerShell falls back to `notepad`; bash falls back to `vi`. Editor must exit non-zero when the user aborts (any sane editor does).
- Inno Setup 6 + Visual Studio 2022 with the C++ desktop workload are needed only for the **CI** build. Your local machine does not need them to cut a release.

## Cut a release

```bash
# Windows
./script/bump-fork-version.ps1

# POSIX
./script/bump-fork-version.sh
```

The script does everything: preconditions → editor for release notes → bump `crates/zed/Cargo.toml` and `Cargo.lock` → commit → tag `v<new>` → push `--follow-tags`.

GitHub Actions picks up the tag and runs `.github/workflows/release-fork.yml`. ~15-20 min on `windows-latest`. Watch the run at `https://github.com/<org>/<fork>/actions`.

On success the workflow:

1. Builds `Zed-Fork-x86_64.exe` via `script/bundle-windows.ps1`.
2. Emits `SHA256SUMS.txt` via `sha256sum -b *.exe`.
3. Extracts release notes from the bump commit body via `git show -s --format=%b "$GITHUB_REF_NAME"`.
4. Publishes a GitHub Release with installer + `SHA256SUMS.txt` + body via `softprops/action-gh-release@v2`. `fail_on_unmatched_files: true` so a missing artifact loud-fails the workflow.

The intranet pull worker mirrors the release within its poll interval (default 5 min). Once mirrored, fork clients pointed at the intranet URL detect, SHA-verify, and install the new version.

## What the preconditions enforce

The bump script's Step 0 will hard-fail before touching `Cargo.toml` if any of these hold:

- **Working tree or staged area not clean** — keeps WIP out of the release commit.
- **Branch is not `perforce-integration` or `fork-update-system`** — release tags must originate from the release branch.
- **`git fetch origin <branch>` fails** — push will fail anyway; better to know now.
- **Local is behind origin** — non-fast-forward push would leave a tag that points at a commit not on origin.
- **Local or remote tag `v<new>` already exists** — tags are immutable in this workflow.
- **`cargo` or `git` missing** — tool sanity.

Fail-early means nothing has been mutated when the script exits non-zero. Rerun after fixing.

## Release notes constraints

The editor template is:

```
# Write release notes for v<new>. Lines starting with # are ignored.
# Highlights: 2-5 user-visible bullets, English, one per line.
# Plain ASCII only (non-ASCII chars rejected by this script).
# If no new features, a single line 'Stability & maintenance.' is fine.

## Highlights
-

## Known issues
-
```

- **English ASCII only.** A single non-ASCII byte trips the script and again the CI re-check. Both reject; both clearly say why. There is no encoding-detection / transliteration fallback by design — the `git show -s --format=%b` → GitHub Release body → intranet `notes.md` pipeline is byte-transparent, and any non-ASCII risks confusing one of the encoders along that path.
- **Empty body is rejected.** Comment lines (`#`-prefixed) are stripped before the check, so the template's instructional comments do not count as content.
- **Format**: a `## Highlights` block with 2-5 bullets, plus an optional `## Known issues` block. Other Markdown is fine but tools downstream render it as-is.

## Typo recovery: "速救 patch" (super-patch)

Tags are immutable. If you ship `vX.Y.Z-fork.5` with a typo or wrong release notes, do NOT delete and recreate the tag. Cut a new `vX.Y.Z-fork.6` with corrected notes that say something like `Supersedes vX.Y.Z-fork.5: <what was wrong>`.

Reason: GitHub serves cached release URLs, intranet workers may have already pulled the old release, and clients may have already SHA-verified the old binary. Rewriting an existing tag means N different machines now disagree on what `vX.Y.Z-fork.5` is.

## When CI fails

| Failing step | What to check |
|---|---|
| Rust install / cache | `rust-toolchain.toml` — does the pinned channel still exist on `windows-latest`? |
| `script/bundle-windows.ps1` | Usually VS edition probe (the script tries Enterprise → Professional → Community → BuildTools) or a missing dependency. Check the step log. |
| Hash artefacts | `bundle-windows.ps1` did not produce an installer — look further up the log for the real failure. |
| Extract release notes | The bump commit body was empty or contained non-ASCII characters. Should be impossible if the bump script was used; cut a new `fork.{N+1}`. |
| Publish GitHub Release | `softprops/action-gh-release` printed `fail_on_unmatched_files: true` and listed the missing path — fix the producing step. |

If the workflow succeeds but the intranet client does not see the new release within the worker poll interval:

- Check the intranet worker: `journalctl -u zed-fork-update-worker -n 100`.
- Confirm the worker's GitHub token still has read access to the fork repo.

## What humans never do

- **Force-push** `perforce-integration` or `fork-update-system`.
- **Delete and recreate** an existing tag.
- **Hand-edit** the published release notes on GitHub. The intranet `notes.md` mirror is generated from the GitHub Release body via the worker, but only at pull time; later edits do not propagate without manual intervention.
