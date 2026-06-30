#!/usr/bin/env pwsh
# bump-fork-version.ps1 — drive a fork release.
#
# Spec ref: docs/fork-update-system.spec.md  10.1
#
# What it does:
#   Step 0  Preconditions (fail-early; do NOT touch Cargo.toml until all pass)
#   Step 1  Parse current version, compute next "<base>-fork.<N+1>"
#   Step 2  Open $EDITOR for release notes (template with comment lines)
#   Step 3  Strip comment lines + ASCII-only check + non-empty check
#   Step 4  Bump crates/zed/Cargo.toml + Cargo.lock to the new version
#   Step 5  git commit "Bump to <new>\n\n<notes>"
#   Step 6  git tag v<new>
#   Step 7  git push --follow-tags origin <branch>
#
# Required tools: git, cargo. cargo-edit (cargo set-version) is preferred but
# optional; if absent we fall back to a regex rewrite of crates/zed/Cargo.toml
# and Cargo.lock.
#
# Rationale notes:
#   - Fail-early preconditions: a half-mutated Cargo.toml after, say, a remote
#     tag collision is harder to clean up than refusing to start. We check
#     everything we can without writing first.
#   - UTF-8 release notes (no BOM): the notes flow through
#         git commit -F (UTF-8 bytes)  ->  git show -s --format=%b  ->
#         softprops/action-gh-release body  ->  GitHub Release body  ->
#         intranet worker (gh API .body, Go json UTF-8)  ->  notes.md  ->
#         nginx (text/markdown; charset=utf-8)  ->  Zed client markdown
#     Every hop is byte-clean for UTF-8, so non-ASCII (Chinese) is safe.
#     We only require valid UTF-8 (BOM stripped on read) and reject invalid
#     byte sequences; non-ASCII printable chars are allowed.
#   - Branch acceptance: `main` is the only long-lived branch (CLAUDE.md
#     Rule 5). Releases always tag a commit on `main`. The old landing-zone
#     branches `perforce-integration` and `fork-update-system` are still
#     accepted for now as a transitional escape hatch in case someone is
#     still working on a worktree against an old checkout; once those branch
#     refs are deleted from the remote, drop those two arms.

$ErrorActionPreference = 'Stop'
Set-Location (git rev-parse --show-toplevel)

function Fail($msg) { Write-Error $msg; exit 1 }

# ---------------------------------------------------------------------------
# Step 0  Preconditions
# ---------------------------------------------------------------------------

git diff --quiet
if ($LASTEXITCODE -ne 0) { Fail "Working tree has unstaged changes." }
git diff --cached --quiet
if ($LASTEXITCODE -ne 0) { Fail "Staged changes present." }

$branch = (git rev-parse --abbrev-ref HEAD).Trim()
if ($branch -ne 'main' -and $branch -ne 'perforce-integration' -and $branch -ne 'fork-update-system') {
    Fail "Must run on main (CLAUDE.md Rule 5) or one of the legacy branches perforce-integration / fork-update-system (current: $branch)."
}

git fetch origin $branch | Out-Null
if ($LASTEXITCODE -ne 0) { Fail "git fetch origin $branch failed." }

$behind = (git rev-list "HEAD..origin/$branch" | Measure-Object).Count
if ($behind -gt 0) { Fail "Local is $behind commits behind origin/$branch. Pull first." }

foreach ($tool in 'cargo', 'git') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { Fail "$tool not found in PATH." }
}

# cargo-edit detection: probe for the `cargo set-version` subcommand.
$haveCargoEdit = $false
& cargo set-version --help *>$null
if ($LASTEXITCODE -eq 0) { $haveCargoEdit = $true }

# ---------------------------------------------------------------------------
# Step 1  Compute new version
# ---------------------------------------------------------------------------

$cargoPath = 'crates/zed/Cargo.toml'
$cargoRaw = Get-Content $cargoPath -Raw
$verMatch = [regex]::Match($cargoRaw, '(?m)^version\s*=\s*"([^"]+)"')
if (-not $verMatch.Success) { Fail "Could not find [package].version in $cargoPath." }
$current = $verMatch.Groups[1].Value

if ($current -notmatch '^(\d+\.\d+\.\d+)-fork\.(\d+)$') {
    Fail "Current version '$current' does not match '<x.y.z>-fork.<N>' pattern."
}
$base = $Matches[1]
$forkN = [int]$Matches[2]
$new = "$base-fork.$($forkN + 1)"
$newTag = "v$new"

# Local tag must not exist.
& git rev-parse --verify --quiet $newTag *>$null
if ($LASTEXITCODE -eq 0) { Fail "Local tag $newTag already exists." }

# Remote tag must not exist.
$remoteTag = (git ls-remote --tags origin $newTag)
if ($remoteTag) { Fail "Remote tag $newTag already exists." }

# ---------------------------------------------------------------------------
# Step 2  Open editor for release notes
# ---------------------------------------------------------------------------

$notesFile = New-TemporaryFile
$template = @"
# Write release notes for $newTag. Lines starting with # are ignored.
# Highlights: 2-5 user-visible bullets, English, one per line.
# Plain ASCII only (non-ASCII chars rejected by this script).
# If no new features, a single line 'Stability & maintenance.' is fine.

## Highlights
-

## Known issues
-
"@

# UTF-8 no-BOM (PS Set-Content -Encoding UTF8 writes a BOM on Windows
# PowerShell 5.1; use .NET to be safe across hosts).
[System.IO.File]::WriteAllText($notesFile.FullName, $template, (New-Object System.Text.UTF8Encoding($false)))

$editor = $env:GIT_EDITOR
if (-not $editor) { $editor = $env:EDITOR }
if (-not $editor) { $editor = 'notepad' }

& $editor $notesFile.FullName
if ($LASTEXITCODE -ne 0) { Fail "Editor exited non-zero." }

$rawNotes = [System.IO.File]::ReadAllText($notesFile.FullName, [System.Text.Encoding]::UTF8)
Remove-Item $notesFile.FullName

# ---------------------------------------------------------------------------
# Step 3  Strip comments, ASCII check, empty check
# ---------------------------------------------------------------------------

$lines = $rawNotes -split "`r?`n"
$kept = $lines | Where-Object { $_ -notmatch '^\s*#' }
$notes = ($kept -join "`n").Trim()

if ($notes -match '[^\x00-\x7F]') { Fail "Release notes contain non-ASCII characters." }
if (-not $notes) { Fail "Release notes are empty after stripping comment lines." }

# ---------------------------------------------------------------------------
# Step 4  Bump Cargo.toml + Cargo.lock
# ---------------------------------------------------------------------------

if ($haveCargoEdit) {
    & cargo set-version -p zed $new
    if ($LASTEXITCODE -ne 0) { Fail "cargo set-version failed." }
} else {
    # Regex fallback: rewrite the [package].version line in crates/zed/Cargo.toml,
    # then update the [[package]] name = "zed" entry's version in Cargo.lock.
    $newCargo = [regex]::Replace(
        $cargoRaw,
        '(?m)^(version\s*=\s*")[^"]+(")',
        "`${1}$new`${2}",
        1)
    [System.IO.File]::WriteAllText($cargoPath, $newCargo, (New-Object System.Text.UTF8Encoding($false)))

    $lockPath = 'Cargo.lock'
    $lockRaw = [System.IO.File]::ReadAllText($lockPath)
    # Match the `name = "zed"` entry followed by its `version = "..."` line.
    # Cargo.lock has many [[package]] blocks; we only rewrite the one whose name is "zed".
    $lockPattern = '(?ms)(\[\[package\]\]\s*\n\s*name\s*=\s*"zed"\s*\n\s*version\s*=\s*")[^"]+(")'
    if ($lockRaw -notmatch $lockPattern) { Fail "Could not find zed entry in Cargo.lock." }
    $newLock = [regex]::Replace($lockRaw, $lockPattern, "`${1}$new`${2}", 1)
    [System.IO.File]::WriteAllText($lockPath, $newLock)
}

# ---------------------------------------------------------------------------
# Steps 5-7  Commit, tag, push
# ---------------------------------------------------------------------------

git add $cargoPath Cargo.lock
if ($LASTEXITCODE -ne 0) { Fail "git add failed." }

$commitMsg = "Bump to $new`n`n$notes"
git commit -m $commitMsg
if ($LASTEXITCODE -ne 0) { Fail "git commit failed." }

git tag $newTag
if ($LASTEXITCODE -ne 0) { Fail "git tag $newTag failed." }

git push --follow-tags origin $branch
if ($LASTEXITCODE -ne 0) { Fail "git push --follow-tags failed. Local commit + tag are still in place; rerun the push manually." }

Write-Host "Bumped to $new and pushed $newTag. CI will build and publish the release."
