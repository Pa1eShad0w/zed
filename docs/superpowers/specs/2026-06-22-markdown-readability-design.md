# Markdown Preview/Agent Readability — Design

Date: 2026-06-22
Branch: `markdown-readability` (worktree off `perforce-integration`)
Crate: `crates/markdown`

## Problem

Zed's native markdown rendering is hard to read for CJK-heavy content compared to
VSCode's markdown preview. Two concrete defects, observed by comparing the same
document rendered in both:

1. **Body line height too tight.** Paragraphs and list items render at
   `line_height(rems(1.3))` (hardcoded in the element builder). For CJK glyphs
   (tall, dense) this packs wrapped lines together. VSCode preview uses ~1.6.
2. **Inline code unreadable.** Inline `code` spans use a faint background
   (`editor_foreground.opacity(0.08)`) and have **no horizontal padding**, so code
   collides visually with adjacent CJK text.

## Non-goals (YAGNI)

- Fenced code blocks (already have border + bg + 8px padding — acceptable).
- List-item / paragraph *margins* (inter-block spacing) — out of scope this pass.
- Exposing any of this as a user setting. All values hardcoded.
- Rounded corners on inline code (requires custom paint; deferred).
- Touching GPUI's shared text system.

## Constraints

- **Rule 1 (no breaking user-space):** must not touch the GPUI text system
  (`crates/gpui/src/text_system/*`) — it serves the editor selection/search
  highlights, terminal, every label. Must not touch the git backend.
- **Rule 2 (TDD):** failing test first; all existing tests must pass; do not edit
  existing tests.

## Key facts established by code reading

- Style is built once in `MarkdownStyle::themed_with_overrides`
  (`markdown.rs:160-310`) and `with_preview_overrides` (`markdown.rs:312-351`),
  stored in `MarkdownStyle`, read by the builder at render. **Style edits cost
  nothing at render time** (no re-parse, no extra per-frame work).
- Body line height is **not** the `base_text_style.line_height` (= `buffer_font_size
  * 1.75`); it is overridden per-block in the builder:
  - paragraph: `markdown.rs:1359` `el.mb_2().line_height(rems(1.3))`
  - list item: `markdown.rs:1589` `el.mb_1().gap_1().line_height(rems(1.3))`
- Inline code is rendered as a **styled text run inside one monolithic
  `StyledText`** per paragraph (`push_markdown_code_span:1282` →
  `push_text:3195` → `flush_text:3337`). It is **not** a separate element, so it
  cannot receive box padding directly.
- Inline-code background is painted by GPUI as a flat `fill()` quad bounding the
  glyphs (`gpui/.../text_system/line.rs:630`): no padding, no corner radius.
- **Copy/selection use SOURCE offsets, not rendered text:** `Markdown::copy`
  (`markdown.rs:844`) and `selected_text` (`markdown.rs:802`) map the selection back
  to the original markdown source. Characters that exist only in the rendered text
  (with no source mapping) never reach the clipboard.
- The rendered→source mapping already tolerates "rendered-only" characters: a gap
  where `prev_source_end < mapping.source_index` is handled explicitly
  (`source_index_for_exclusive_rendered_end:3421-3430`).

## Approach (chosen: "C")

Inline-code padding is achieved by **inserting synthetic whitespace** around the
code text, carrying the inline-code background run, with **no source mapping** for
the synthetic characters. The existing flat background paints behind them, giving
left/right breathing room. No GPUI change, no custom paint, no rounded corners.

Magnitude differs by surface (like `heading_level_styles` already does):
- **Agent panel:** gentle — thin space (`\u{2009}`) each side, bg opacity `0.12`.
- **Preview ("完全体"):** stronger — wider space (`\u{2002}`) each side, bg opacity ~`0.16`.

Body line height fix is scoped to **preview only** via a new style field, leaving
the agent panel unchanged.

## Changes

| # | Location | Change |
|---|----------|--------|
| 1 | `MarkdownStyle` struct (`markdown.rs:95-115`) | Add `paragraph_line_height: Rems`; add `inline_code_padding: SharedString` (the per-side whitespace string, empty = none) |
| 2 | `Default for MarkdownStyle` (`markdown.rs:117-141`) | Defaults: `paragraph_line_height: rems(1.3)`, `inline_code_padding: ""` |
| 3 | `themed_with_overrides` (`markdown.rs:206-303`) | Agent/Editor: `paragraph_line_height = rems(1.3)`, inline_code bg `0.08 → 0.12`, `inline_code_padding = "\u{2009}"` |
| 4 | `with_preview_overrides` (`markdown.rs:312-351`) | Preview: `paragraph_line_height = rems(1.6)`, inline_code bg ~`0.16`, `inline_code_padding = "\u{2002}"` |
| 5 | builder paragraph + list item (`markdown.rs:1359, 1589`) | Replace `rems(1.3)` with `self.style.paragraph_line_height` |
| 6 | `push_markdown_code_span` (`markdown.rs:1282`) | Wrap code text with the `inline_code_padding` string on each side, styled with the inline-code background run, appended **without** a source mapping; the real code text still goes through `push_text` with its true source range |

## Data flow

```
themed_with_overrides / with_preview_overrides
  → sets MarkdownStyle.paragraph_line_height, .inline_code(bg), .inline_code_padding
    → builder reads self.style at render
       → paragraph/list-item div line_height = paragraph_line_height
       → code span: [pad-run][code text runs][pad-run] inside the StyledText
          → copy/selection map via source mappings → pad chars excluded
```

No re-parse on style change. Added per-frame cost = O(inline code spans), negligible.

## Risk

- Only real risk: synthetic whitespace desyncing `source_mappings` (selection,
  search highlight, copy). Mitigated by appending pad chars with no source mapping
  and relying on the existing gap-tolerant mapping logic. **Guarded by tests.**
- GPUI untouched; git backend untouched → no breaking-user-space exposure.

## Test plan (TDD: red first)

New tests (`crates/markdown` test module):
1. Selecting an inline-code span and copying returns the **exact** code text with
   no phantom padding whitespace (guards change #6 / source mapping).
2. Selecting a range that spans inline code + surrounding text maps to the correct
   source substring (no off-by-N from synthetic chars).
3. Preview `paragraph_line_height` flows into the rendered line layout
   (assert the rendered line height reflects `rems(1.6)` for preview vs `rems(1.3)`
   for agent), to the extent the layout exposes it.

Existing tests that MUST stay green (not edited):
- `test_inline_code_word_selection_excludes_backticks` (`markdown.rs:4331`)
- word-selection test (`markdown.rs:4027`)
- all other `crates/markdown` tests.

## Verification

- `cargo test -p markdown` green (baseline + new).
- `/linus-review` on the diff (Rule 1) — focus data/control flow, breaking-user-space.
- Manual visual check in a debug build is optional; not required for merge.

## Workflow

- All work in worktree `.worktrees/markdown-readability` (branch
  `markdown-readability`), isolated from the dirty `perforce-integration` checkout.
- Native git worktree (not junction/symlink) per the Windows safety rule.
