---
name: merge-upstream
description: Merge an official stable upstream Zed release into this Perforce fork, preserve upstream and fork behavior, validate the candidate, and prepare a confirmed fork release. Use for upstream uptake, pre-merge status, conflict review, or release readiness after uptake.
---

# Merge upstream into the Zed Perforce fork

## Scope and sources

Run Git commands from the fork repository (zed-src in the parent workspace). Read applicable AGENTS.md, CLAUDE.md and .rules. Inspect CHANGELOG.fork.md, FORK-MERGE-BLOCKERS.md, docs/release-process.fork.md, script/bump-fork-version.ps1 or .sh, and .github/workflows/release-fork.yml before release work.

main is the only long-lived branch. Use a temporary uptake/vX.Y.Z branch; never merge upstream/main, rebase published commits, force-push, or move published tags. The upstream zed-cherry-pick skill describes a different release model and does not govern this fork.

Keep this file identical in .agents/skills/merge-upstream and .claude/skills/merge-upstream. Parent-workspace discovery entries forward here; the workflow itself travels with the fork repository.

## 1. Record pre-merge status

- Inspect status (including untracked files), worktrees, current branch, tracking branch, remotes and local/remote fork tags. Preserve unrelated work. main should track origin/main.
- Fetch the relevant origin branch and the chosen upstream tag without force or automatic merging. Record old main, origin/main, previous upstream base and target tag's peeled commit SHA.
- Verify the target is an official published stable release, not a draft or prerelease, using upstream release metadata. Record the source URL and check date. A version-shaped tag name alone is insufficient; do not assume the latest stable version.
- Inspect ancestry and merge base. If the previous stable tag is not an ancestor, inspect old-only changes with git cherry / patch equivalence and content review. Do not equate different SHAs with missing fixes, or equivalent patches with complete behavioral proof.
- Compare upstream release notes and changes with the fork's current feature inventory. Report ahead/behind, actual merge base, expected conflict areas, open blockers, existing test failures/skips and the proposed version.
- Establish a focused old-main baseline for affected behavior before changing it. Record toolchain, environment and commands so pre-existing failures remain distinguishable from regressions.

## 2. Build an isolated candidate

Create uptake/vX.Y.Z from the recorded main, preferably in a separate worktree. Merge the verified upstream commit with --no-ff; use --no-commit to inspect the result before recording the merge.

These are invariant checks, NOT an exhaustive conflict list:
- crates/zed/RELEASE_CHANNEL remains fork.
- crates/zed/Cargo.toml uses the agreed X.Y.Z-fork.N.
- The zed package in Cargo.lock has exactly the same version.

Inventory every conflict and inspect semantic changes in automatically merged files. Compare the final candidate against BOTH old main (what users lose/change) and new upstream (what the fork overrides). A clean textual merge does not prove compatibility.

Preserve both sides' intended behavior. For confirmed regressions, add a failing regression test, fix the candidate, then rerun relevant checks. Repeatedly discarding and re-merging the same inputs does not repair a semantic regression. Keep main unchanged until the candidate passes. Abort/recreate only when warranted, preserving unrelated work and useful evidence.

Existing tests must not be edited, removed, ignored or weakened without the project's required explicit human approval. Explain the precise fixture/API/default mismatch and the assertions to retain. Check existing session authorization before asking again. Adding a new regression test follows red -> green. Do not change production defaults merely to make upstream fixtures pass.

## 3. Review sensitive behavior

Apply /linus-review to the merge and subsequent fixes, including data flow, control flow and break surfaces. Confirm suspected bugs before reporting and fixing them. Use a change-specific matrix; the following are this fork's recurring sensitive areas, not a claim that every uptake touches all of them.

| Surface | Behavior to preserve/check when affected |
| --- | --- |
| Git and P4 discovery | Default Git priority, configured P4 preference, ordinary folders, nested repositories, scan depth, ignore rules, parked repository activation |
| Buffer lifecycle | Read-only/checkout permissions, edit subscriptions, deduplication, cleanup, async completion, no entity re-entry during activation |
| P4 content and history | Raw bytes until shared decoding, non-UTF-8 content, diff/history/annotate, revision labels across local/remote serialization |
| Git-only actions | P4 synthetic revisions never invoke unsupported Git operations; real Git revision actions remain available |
| Agent | Title language and session identity, user/external title precedence, retry, scheduled-message initialization/recovery/cancel/send |
| Markdown | Exact copied text, source mapping and selection, inline code versus fenced blocks, approved visual substitutions |
| File operations | Undo/redo preserves file contents and unrelated paths |
| Fork distribution | Fork channel, version, server configuration, update integrity, telemetry default-off and fork transport isolation |

Record user-approved behavior changes precisely. For example, approval to replace inline-code side padding with upstream chip painting does not authorize unrelated fenced-code, copying or selection changes.

## 4. Validate the final candidate

- Use the pinned rust-toolchain.toml and locked dependencies. Run focused tests first, then the workspace suite for a broad upstream uptake, using current repository/CI test instructions.
- Run script/clippy --locked with the repository's strict settings, and cargo build --locked --release -p zed. Compilation alone is not a smoke test.
- Start the resulting application with isolated user data; verify build identity, first frame and representative Git discovery without disturbing the user's editor. Record startup errors and the limits of the check. Test real P4 and installer/update paths when available or explicitly report the gap.
- Log exact candidate SHA/tree, commands, toolchain, results and artifact identity. Separate passed, failed, timed-out and skipped tests. Skipped tests are not passes. Account for every initial failure with a fix, evidence-backed baseline explanation, or a passing rerun.
- Resolve open release blockers before declaring release readiness. Do not hide failures by changing assertions, disabling checks or broadening timeouts.
- After changes, rerun the affected checks. Bind the final build/smoke result to the final source; earlier builds cannot validate later production edits. Documentation-only changes can reuse source validation with an explicit tree-difference explanation.

Windows notes, only when relevant: use the Visual Studio environment and MSVC linker, not Git Bash's link.exe. Match CI services and extension SDK prerequisites. Diagnose PostgreSQL localhost/IPv6 connectivity and cold extension dependency downloads before attributing timeouts to code. Scope any temporary service/proxy to the task and remove it after use. Never silently skip a failing test.

## 5. Land locally

Confirm main has not changed since candidate creation. If it has, integrate and revalidate the resulting candidate; do not reset other work.

Merge the validated uptake into main with --no-ff. Verify old main and upstream SHA are ancestors, and compare the landed tree with the validated tree. Remove only the merged temporary branch/worktree, retaining logs. Do not force-delete unrelated files or bypass a denied cleanup operation.

Record material user-facing changes in CHANGELOG.fork.md using Chinese entries, English categories, real commit SHAs and dates. Documentation-only skill changes do not need changelog entries. Preserve published history; append commits rather than rewriting it.

## 6. Prepare and confirm publication

Local landing and publication are separate actions. Honor any existing explicit publication authorization; otherwise prepare a concrete proposal and obtain the user's requested confirmation before commit/push/tag actions covered by that confirmation. Include version, exact remote/refs, candidate identity, release commit message/body, validation gaps and automation side effects.

Read the current scripts and workflow rather than trusting stale runbook prose:
- Current bump scripts increment fork.N, commit, create a lightweight tag and push in one invocation. They are not preparation/dry-run commands.
- An uptake may already contain the intended first unpublished fork version. Do not accidentally increment it again. Explicitly confirm whether to release that version with equivalent manual checks or run the script to increment it.
- The current release workflow reads the TAGGED COMMIT BODY for notes. Prepare nonempty English ASCII notes there; an annotated-tag message or empty merge-commit body does not satisfy it.
- Check tag uniqueness locally and remotely, main ancestry, clean status, matching manifest/lockfile/tag versions and origin/main freshness immediately before publication.
- Push only the approved branch and exact tag, explicitly. Lightweight tags are not sent by --follow-tags. Never use --all, --tags or force for this workflow.
- Pushing a matching fork tag triggers the Windows build and GitHub Release, then intranet mirroring and configured client updates. This is a publication action.
- For workflow_dispatch validation, verify checkout ref equals the intended source SHA. A tag input alone does not select the build's checkout ref.
- Verify remote branch/tag SHAs, CI result, release notes and installer/debug-symbol/checksum assets. Distinguish tag pushed, release built, mirror updated and client update verified.
- On partial failure, inspect local/remote refs and resume the same approved operation where safe. Do not create another version or delete/recreate a published tag to conceal the failure. Published fixes use a new fork.N.

## Report

Provide the stable source/base snapshot, preserved and approved-changed behavior, data/control-flow review and break surfaces, validation with remaining gaps, local/remote publication status, and any concrete approval still needed. Never promise that testing proves there can be no special-case failures.
