# GitHub Actions Setup (Fork)

One-time runbook for the maintainer adopting this fork in a new GitHub organisation. After the first time, day-to-day release work is in [release-process.fork.md](release-process.fork.md).

## Branch protection

Recommended for `perforce-integration` (or whatever the live release branch is named in your org):

- **Require linear history** if your workflow allows it. This makes the relationship between bump commits and tags one-to-one.
- **Disallow force-push by everyone except maintainers.** Force-push to a branch that has tags pointing into it would orphan releases.
- **Require status checks before merge** — outside the scope of this fork-update-system spec, but recommended for PR-driven workflows. Match whatever the rest of your org does.

`release-fork.yml` itself does not require any branch protection to function. It triggers on tag push, not branch push.

## Permissions and secrets

`.github/workflows/release-fork.yml` needs:

- **`GITHUB_TOKEN`** — implicitly provided by GitHub Actions. No setup required.
- **`permissions: contents: write`** — set at the job level inside the workflow. No org-side configuration needed beyond that.

It does NOT need:

- A code-signing certificate. Fork releases are intentionally unsigned; SHA-256 verify at the client + worker is the trust anchor.
- An Azure Trusted Signing tenant ID, certificate profile, or endpoint.
- A Sentry auth token.

The workflow clears `$env:CI` before invoking `script/bundle-windows.ps1` so the upstream script's signing / telemetry-upload branches all short-circuit. If you later decide to sign the fork installer, do NOT just re-enable `$env:CI` — the script will demand the upstream signing secrets. Add a fork-specific signing step instead.

## Runner environment

The workflow runs on `windows-latest`, which currently provides:

- **Visual Studio 2022 Enterprise** with the Desktop C++ workload (path: `C:\Program Files\Microsoft Visual Studio\2022\Enterprise\...`). `script/bundle-windows.ps1` probes editions in this order: Enterprise → Professional → Community → BuildTools; the first that has `Common7\Tools\Launch-VsDevShell.ps1` wins.
- **Inno Setup 6** (`iscc.exe` on `PATH`).
- **Rust toolchain** resolved automatically by `rust-toolchain.toml` in the repo root via `rustup show`.
- **`Swatinem/rust-cache@v2`** caches `~/.cargo` and the target dir keyed on `Cargo.lock`. First run is slow (full build); subsequent runs reuse the cache.

If the GitHub-hosted runner image stops shipping VS 2022, the workflow fails at the bundle step with a clear error from the probe loop. Either:

- Use a self-hosted runner with VS 2022 installed, or
- Update the probe loop in `script/bundle-windows.ps1` to handle a newer VS version.

## What this workflow does not do

- **Does not install `cargo-edit`** — the bump scripts can fall back to a regex rewrite.
- **Does not push to `origin`** — only humans push tags, via the bump script. The CI is read-only against the repo's git ref (it reads via `actions/checkout`).
- **Does not auto-merge PRs.**
- **Does not publish anywhere outside GitHub Releases** — the intranet pull worker is what mirrors releases inward.
- **Does not run tests on tag push.** The assumption is that the tag is cut from a branch that was already green. Add a `cargo test --workspace` step to the workflow if you want extra paranoia.

## Manual run / re-trigger

The workflow has `on: push: tags:` only — no `workflow_dispatch`. To force a re-run for an existing tag:

```bash
git push origin :v<tag>
git push origin v<tag>
```

This is **strongly discouraged** for any tag whose Release has already been published — see "Typo recovery" in [release-process.fork.md](release-process.fork.md). Cut a new `fork.{N+1}` instead.

## Logs and debugging

- Workflow runs: `https://github.com/<org>/<fork>/actions/workflows/release-fork.yml`
- Step logs: click into a run, then the failing job, then the step. PowerShell verbose output is in the step's stdout.
- Cached cargo state: `Swatinem/rust-cache` prints a cache hit/miss line near the top of the run; cache key is `${{ runner.os }}-cargo-${{ hashFiles('Cargo.lock') }}`.

## Renaming

`name:` in the YAML is purely cosmetic; GitHub Actions routes on the file path. If you want to rename the workflow on the Actions UI without renaming the file, edit only `name:`.

The file path `.github/workflows/release-fork.yml` is what GitHub uses to identify the workflow for status badges and external links — change it carefully and update any downstream references.

## Renaming the org / repo

If you fork-of-fork this repo into a new org:

1. Push the rename to GitHub.
2. Update the intranet pull worker's repo coordinates (out of fork-update-system spec; check its config).
3. Update any documentation that links to `https://github.com/<org>/<fork>/`.

The workflow itself uses `${{ github.repository }}` and `${{ github.ref_name }}` so it does not need any change on rename.
