#!/usr/bin/env bash
# bump-fork-version.sh — drive a fork release.
#
# Spec ref: docs/fork-update-system.spec.md  10.1
#
# POSIX equivalent of bump-fork-version.ps1. Same flow:
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
# optional; if absent we fall back to an in-place rewrite of
# crates/zed/Cargo.toml and Cargo.lock via tmp-file + mv.
#
# Rationale notes:
#   - Fail-early preconditions: a half-mutated Cargo.toml after, say, a remote
#     tag collision is harder to clean up than refusing to start. We check
#     everything we can without writing first.
#   - ASCII-only release notes: the notes flow through
#         git commit -m  ->  git show -s --format=%b  ->  CI release body
#                            ->  intranet worker (gh api .body)  ->  notes.md
#     Any non-ASCII char risks getting mangled at one of those hops. We refuse
#     non-ASCII rather than chase an encoding bug 6 months later.
#   - Branch acceptance: the script currently lives on `fork-update-system`
#     and will eventually merge into `perforce-integration`. Until that merge
#     we accept either branch name so the script is usable during migration.
#     After the merge, drop the `fork-update-system` arm.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

fail() { echo "error: $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Step 0  Preconditions
# ---------------------------------------------------------------------------

git diff --quiet || fail "Working tree has unstaged changes."
git diff --cached --quiet || fail "Staged changes present."

branch=$(git rev-parse --abbrev-ref HEAD)
if [ "$branch" != "perforce-integration" ] && [ "$branch" != "fork-update-system" ]; then
    fail "Must run on perforce-integration or fork-update-system (current: $branch)."
fi

git fetch origin "$branch" >/dev/null || fail "git fetch origin $branch failed."

behind=$(git rev-list "HEAD..origin/$branch" | wc -l | tr -d ' ')
[ "$behind" -eq 0 ] || fail "Local is $behind commits behind origin/$branch. Pull first."

for t in cargo git; do
    command -v "$t" >/dev/null 2>&1 || fail "$t not found in PATH."
done

# cargo-edit detection (optional).
have_cargo_edit=0
if cargo set-version --help >/dev/null 2>&1; then
    have_cargo_edit=1
fi

# ---------------------------------------------------------------------------
# Step 1  Compute new version
# ---------------------------------------------------------------------------

cargo_path='crates/zed/Cargo.toml'
current=$(awk -F'"' '/^version[[:space:]]*=[[:space:]]*"/ {print $2; exit}' "$cargo_path")
[ -n "$current" ] || fail "Could not find [package].version in $cargo_path."

if [[ ! "$current" =~ ^([0-9]+\.[0-9]+\.[0-9]+)-fork\.([0-9]+)$ ]]; then
    fail "Current version '$current' does not match '<x.y.z>-fork.<N>' pattern."
fi
base="${BASH_REMATCH[1]}"
forkN="${BASH_REMATCH[2]}"
new="${base}-fork.$((forkN + 1))"
new_tag="v$new"

if git rev-parse --verify --quiet "$new_tag" >/dev/null 2>&1; then
    fail "Local tag $new_tag already exists."
fi
if [ -n "$(git ls-remote --tags origin "$new_tag")" ]; then
    fail "Remote tag $new_tag already exists."
fi

# ---------------------------------------------------------------------------
# Step 2  Open editor for release notes
# ---------------------------------------------------------------------------

notes_file=$(mktemp)
# Best-effort cleanup: if we exit before the explicit rm, drop the temp file.
trap 'rm -f "$notes_file"' EXIT

cat > "$notes_file" <<EOF
# Write release notes for $new_tag. Lines starting with # are ignored.
# Highlights: 2-5 user-visible bullets, English, one per line.
# Plain ASCII only (non-ASCII chars rejected by this script).
# If no new features, a single line 'Stability & maintenance.' is fine.

## Highlights
-

## Known issues
-
EOF

editor="${GIT_EDITOR:-${EDITOR:-vi}}"
# shellcheck disable=SC2086
$editor "$notes_file"

raw_notes=$(cat "$notes_file")

# ---------------------------------------------------------------------------
# Step 3  Strip comments, ASCII check, empty check
# ---------------------------------------------------------------------------

# Drop comment lines, then trim leading/trailing blank lines.
notes=$(printf '%s\n' "$raw_notes" | grep -v '^[[:space:]]*#' || true)
# Trim trailing whitespace.
notes=$(printf '%s' "$notes" | sed -e 's/[[:space:]]*$//')
# Collapse leading + trailing all-blank lines without touching interior blanks.
notes=$(printf '%s' "$notes" | awk 'BEGIN{p=0} { if (p==0 && $0 ~ /^[[:space:]]*$/) next; p=1; print }' | awk 'BEGIN{n=0} {a[n++]=$0} END{ end=n-1; while (end>=0 && a[end] ~ /^[[:space:]]*$/) end--; for (i=0; i<=end; i++) print a[i] }')

if printf '%s' "$notes" | LC_ALL=C grep -P '[^\x00-\x7F]' >/dev/null 2>&1; then
    fail "Release notes contain non-ASCII characters."
fi
if [ -z "$(printf '%s' "$notes" | tr -d '[:space:]')" ]; then
    fail "Release notes are empty after stripping comment lines."
fi

# ---------------------------------------------------------------------------
# Step 4  Bump Cargo.toml + Cargo.lock
# ---------------------------------------------------------------------------

if [ "$have_cargo_edit" -eq 1 ]; then
    cargo set-version -p zed "$new"
else
    # Regex fallback via tmp-file + mv (portable across GNU/BSD sed quirks and
    # native Windows git-bash where `sed -i` semantics vary).

    # crates/zed/Cargo.toml: rewrite the first `version = "..."` line.
    tmp=$(mktemp)
    awk -v new="$new" '
        BEGIN { done = 0 }
        /^version[[:space:]]*=[[:space:]]*"[^"]+"/ && done == 0 {
            sub(/"[^"]+"/, "\"" new "\"")
            done = 1
        }
        { print }
    ' "$cargo_path" > "$tmp"
    mv "$tmp" "$cargo_path"

    # Cargo.lock: rewrite the version line in the [[package]] block whose
    # `name = "zed"`. Tracks a small state machine across lines.
    lock_path='Cargo.lock'
    tmp=$(mktemp)
    awk -v new="$new" '
        BEGIN { in_zed = 0; done = 0 }
        /^\[\[package\]\]/ { in_zed = 0 }
        /^name[[:space:]]*=[[:space:]]*"zed"[[:space:]]*$/ { in_zed = 1 }
        in_zed == 1 && done == 0 && /^version[[:space:]]*=[[:space:]]*"[^"]+"/ {
            sub(/"[^"]+"/, "\"" new "\"")
            done = 1
            in_zed = 0
        }
        { print }
    ' "$lock_path" > "$tmp"
    mv "$tmp" "$lock_path"
fi

# ---------------------------------------------------------------------------
# Steps 5-7  Commit, tag, push
# ---------------------------------------------------------------------------

git add "$cargo_path" Cargo.lock

git commit -m "Bump to $new

$notes"

git tag "$new_tag"

git push --follow-tags origin "$branch"

echo "Bumped to $new and pushed $new_tag. CI will build and publish the release."
