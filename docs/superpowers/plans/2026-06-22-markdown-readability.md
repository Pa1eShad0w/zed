# Markdown Readability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Improve Zed markdown readability — looser body line height (preview only) and inline-code horizontal padding + deeper background (agent gentle, preview full) — without touching GPUI or the git backend.

**Architecture:** Add two fields to `MarkdownStyle` (`paragraph_line_height`, `inline_code_padding`). The element builder reads them at render. Padding is synthetic whitespace appended around inline-code text, styled with the inline-code background run, with **no source mapping** so copy/selection (which round-trip through source offsets) never include it. One edge — code at end of a line — is handled by nudging `current_source_index` past the closing backtick so the rendered-text clamp does not swallow the trailing pad.

**Tech Stack:** Rust, gpui, `crates/markdown`.

**Toolchain note (this machine):** the Bash `cargo`/`rustc` resolve to a standalone 1.90 that CANNOT compile this repo. Always prepend the pinned toolchain bin and reuse the master build cache:
```bash
TC=$(rustup which --toolchain 1.95.0 rustc); TCBIN=$(dirname "$TC")
export CARGO_TARGET_DIR=/c/Users/USERNAME/Repos/zed-perforce-integration/zed-src/target
export PATH="$TCBIN:$PATH"
```
All `cargo` commands below assume these two `export`s are already set in the shell. Run them once per shell. `cmd | tail` hides the real exit code — append `; echo "EXIT=${PIPESTATUS[0]}"`.

**Working dir:** worktree `.worktrees/markdown-readability` (branch `markdown-readability`).

---

## File Structure

Only one source file changes plus its in-file test module:
- Modify: `crates/markdown/src/markdown.rs`
  - `MarkdownStyle` struct + `Default` (new fields)
  - `MarkdownStyle::themed_with_overrides` and `with_preview_overrides` (set values)
  - `MarkdownElement::push_markdown_code_span` (insert padding)
  - `MarkdownElementBuilder` (new `append_styled_no_source` helper; read `paragraph_line_height`)
  - `#[cfg(test)] mod tests` (new helper + new tests)

No new files.

---

## Task 1: Add `MarkdownStyle` fields

**Files:**
- Modify: `crates/markdown/src/markdown.rs` (struct `MarkdownStyle` ~95-115; `impl Default` ~117-141)

- [ ] **Step 1: Add fields to the struct**

In `pub struct MarkdownStyle { ... }`, add these two fields just before the closing `}` (after `table_columns_min_size: bool,`):

```rust
    /// Line height applied to body paragraphs and list items.
    pub paragraph_line_height: DefiniteLength,
    /// Whitespace inserted on each side of inline code spans to give the
    /// highlighted background horizontal breathing room. Empty = none.
    /// These characters are rendered-only (no source mapping) so they never
    /// reach the clipboard or selection.
    pub inline_code_padding: SharedString,
```

- [ ] **Step 2: Add defaults**

In `impl Default for MarkdownStyle`, inside the returned `Self { ... }`, add (after `table_columns_min_size: false,`):

```rust
            paragraph_line_height: rems(1.3).into(),
            inline_code_padding: SharedString::default(),
```

(`rems` and `DefiniteLength` and `SharedString` are already imported and used in this file.)

- [ ] **Step 3: Build**

Run: `cargo build -p markdown 2>&1 | tail -15; echo "EXIT=${PIPESTATUS[0]}"`
Expected: `EXIT=0` (warnings about unused fields are acceptable at this point).

- [ ] **Step 4: Commit**

```bash
git add crates/markdown/src/markdown.rs
git commit -m "feat(markdown): add paragraph_line_height + inline_code_padding style fields"
```

---

## Task 2: Wire `paragraph_line_height` into the builder

**Files:**
- Modify: `crates/markdown/src/markdown.rs` (`push_markdown_paragraph` ~1358-1359; `push_markdown_list_item` ~1589)

- [ ] **Step 1: Replace the hardcoded paragraph line height**

Find in `push_markdown_paragraph`:

```rust
        let mut paragraph = div().when(!self.style.height_is_multiple_of_line_height, |el| {
            el.mb_2().line_height(rems(1.3))
        });
```

Replace the `line_height(rems(1.3))` call so it reads:

```rust
        let mut paragraph = div().when(!self.style.height_is_multiple_of_line_height, |el| {
            el.mb_2().line_height(self.style.paragraph_line_height)
        });
```

- [ ] **Step 2: Replace the hardcoded list-item line height**

Find in `push_markdown_list_item` (the `.when(...)` closure):

```rust
                    el.mb_1().gap_1().line_height(rems(1.3))
```

Replace with:

```rust
                    el.mb_1().gap_1().line_height(self.style.paragraph_line_height)
```

- [ ] **Step 3: Build**

Run: `cargo build -p markdown 2>&1 | tail -15; echo "EXIT=${PIPESTATUS[0]}"`
Expected: `EXIT=0`

- [ ] **Step 4: Run existing tests (no behavior change — default is still rems(1.3))**

Run: `cargo test -p markdown 2>&1 | tail -6; echo "EXIT=${PIPESTATUS[0]}"`
Expected: `test result: ok. 94 passed`, `EXIT=0`

- [ ] **Step 5: Commit**

```bash
git add crates/markdown/src/markdown.rs
git commit -m "feat(markdown): drive body line height from paragraph_line_height field"
```

---

## Task 3: Inline-code padding (TDD — the core change)

**Files:**
- Modify: `crates/markdown/src/markdown.rs`
  - test module: add helper `render_markdown_with_style` + 3 tests
  - `MarkdownElementBuilder`: add `append_styled_no_source`
  - `MarkdownElement::push_markdown_code_span` (~1282-1322): insert padding

### 3a. Tests first

- [ ] **Step 1: Add a style-taking render helper in the test module**

Add this fn next to `render_markdown_with_options` (in `#[cfg(test)] mod tests`):

```rust
    fn render_markdown_with_style(
        markdown: &str,
        style: MarkdownStyle,
        cx: &mut TestAppContext,
    ) -> RenderedText {
        struct StyleTestWindow;

        impl Render for StyleTestWindow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        ensure_theme_initialized(cx);

        let (_, cx) = cx.add_window_view(|_, _| StyleTestWindow);
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                markdown.to_string().into(),
                None,
                None,
                MarkdownOptions::default(),
                cx,
            )
        });
        cx.run_until_parked();
        let (rendered, _) = cx.draw(Default::default(), size(px(600.0), px(600.0)), |_window, _cx| {
            MarkdownElement::new(markdown, style).code_block_renderer(CodeBlockRenderer::Default {
                copy_button_visibility: CopyButtonVisibility::Hidden,
                wrap_button_visibility: WrapButtonVisibility::Hidden,
                border: false,
            })
        });
        rendered.text
    }
```

- [ ] **Step 2: Add the three tests**

Add after `test_inline_code_word_selection_excludes_backticks`:

```rust
    #[gpui::test]
    fn test_inline_code_padding_present_in_rendered(cx: &mut TestAppContext) {
        let style = MarkdownStyle {
            inline_code_padding: "\u{2009}".into(),
            ..MarkdownStyle::default()
        };
        let rendered = render_markdown_with_style("a `b` c", style, cx);
        let line_text = rendered.lines.first().unwrap().layout.text();
        assert!(
            line_text.contains('\u{2009}'),
            "expected thin-space padding around inline code, got {line_text:?}"
        );
    }

    #[gpui::test]
    fn test_inline_code_padding_excluded_from_copy_midline(cx: &mut TestAppContext) {
        let style = MarkdownStyle {
            inline_code_padding: "\u{2009}".into(),
            ..MarkdownStyle::default()
        };
        // "use `blah` here": code content "blah" at source 5..9
        let rendered = render_markdown_with_style("use `blah` here", style, cx);

        // Copy of the code content source range returns exactly "blah" (no pad).
        assert_eq!(rendered.text_for_range(5..9), "blah");

        // Double-click on the code still maps to source 5..9 and copies "blah".
        let word_range = rendered.surrounding_word_range(6);
        assert_eq!(word_range, 5..9);
        assert_eq!(rendered.text_for_range(word_range), "blah");
    }

    #[gpui::test]
    fn test_inline_code_padding_excluded_from_copy_end_of_line(cx: &mut TestAppContext) {
        let style = MarkdownStyle {
            inline_code_padding: "\u{2009}".into(),
            ..MarkdownStyle::default()
        };
        // Code is the last thing on the line — exercises the source_end clamp edge.
        let rendered = render_markdown_with_style("use `blah`", style, cx);
        assert_eq!(rendered.text_for_range(5..9), "blah");
    }
```

- [ ] **Step 3: Run the new tests — expect FAIL (red)**

Run: `cargo test -p markdown inline_code_padding 2>&1 | tail -20; echo "EXIT=${PIPESTATUS[0]}"`
Expected: FAIL — `test_inline_code_padding_present_in_rendered` fails the `contains('\u{2009}')` assertion (padding not yet inserted). The two copy tests pass trivially at this point (no pad inserted yet), which is fine; they become meaningful guards once 3b lands.

### 3b. Implementation

- [ ] **Step 4: Add the `append_styled_no_source` builder helper**

In `impl MarkdownElementBuilder`, directly above `fn push_text`, add:

```rust
    /// Append rendered-only text (e.g. inline-code padding) using the current
    /// text style, WITHOUT recording a source mapping. Because the next real
    /// `push_text` records its mapping at the post-append rendered offset, these
    /// characters fall outside every source range and are excluded from copy and
    /// selection.
    fn append_styled_no_source(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let text_style = self.text_style();
        self.pending_line.text.push_str(text);
        self.pending_line.runs.push(text_style.to_run(text.len()));
    }
```

- [ ] **Step 5: Insert padding in `push_markdown_code_span`**

Replace the body after the `link_url` computation. The current code is:

```rust
        if let Some(url) = link_url {
            builder.push_link(url.clone(), range.clone());
            let link_style = self
                .style
                .link_callback
                .as_ref()
                .and_then(|callback| callback(url.as_ref(), cx))
                .unwrap_or_else(|| self.style.link.clone());
            builder.push_text_style(self.style.inline_code.clone());
            builder.push_text_style(link_style);
            builder.push_text(text, range);
            builder.pop_text_style();
            builder.pop_text_style();
        } else {
            let mut code_style = self.style.inline_code.clone();
            if builder.link_depth > 0 {
                code_style.color = self.style.link.color.or(code_style.color);
            }
            builder.push_text_style(code_style);
            builder.push_text(text, range);
            builder.pop_text_style();
        }
```

Replace it with (padding added on each side, carrying the inline-code background, never the link style; `current_source_index` nudged past the closing backtick to survive the end-of-line clamp):

```rust
        let pad = self.style.inline_code_padding.clone();
        let pad_source_end = range.end + 1;
        if let Some(url) = link_url {
            builder.push_link(url.clone(), range.clone());
            let link_style = self
                .style
                .link_callback
                .as_ref()
                .and_then(|callback| callback(url.as_ref(), cx))
                .unwrap_or_else(|| self.style.link.clone());
            builder.push_text_style(self.style.inline_code.clone());
            builder.append_styled_no_source(&pad);
            builder.push_text_style(link_style);
            builder.push_text(text, range);
            builder.pop_text_style();
            builder.append_styled_no_source(&pad);
            builder.current_source_index = pad_source_end;
            builder.pop_text_style();
        } else {
            let mut code_style = self.style.inline_code.clone();
            if builder.link_depth > 0 {
                code_style.color = self.style.link.color.or(code_style.color);
            }
            builder.push_text_style(code_style);
            builder.append_styled_no_source(&pad);
            builder.push_text(text, range);
            builder.append_styled_no_source(&pad);
            builder.current_source_index = pad_source_end;
            builder.pop_text_style();
        }
```

- [ ] **Step 6: Run the new tests — expect PASS (green)**

Run: `cargo test -p markdown inline_code_padding 2>&1 | tail -20; echo "EXIT=${PIPESTATUS[0]}"`
Expected: all three `inline_code_padding` tests PASS, `EXIT=0`.

- [ ] **Step 7: Run the full crate to confirm no regression**

Run: `cargo test -p markdown 2>&1 | tail -8; echo "EXIT=${PIPESTATUS[0]}"`
Expected: `test result: ok. 97 passed` (94 existing + 3 new), `EXIT=0`. In particular `test_inline_code_word_selection_excludes_backticks` still passes (it uses `MarkdownStyle::default()` whose padding is empty).

- [ ] **Step 8: Commit**

```bash
git add crates/markdown/src/markdown.rs
git commit -m "feat(markdown): inline-code horizontal padding via rendered-only whitespace

Padding chars carry the inline-code background but have no source mapping,
so copy/selection (which round-trip through source offsets) exclude them.
current_source_index is nudged past the closing backtick so end-of-line
code is not swallowed by the rendered-text clamp."
```

---

## Task 4: Set themed values (agent gentle, preview full)

**Files:**
- Modify: `crates/markdown/src/markdown.rs` (`themed_with_overrides` ~206-303; `with_preview_overrides` ~312-351)

- [ ] **Step 1: Agent/editor values in `themed_with_overrides`**

In the `MarkdownStyle { ... }` literal, the `inline_code` field currently has:

```rust
                background_color: Some(colors.editor_foreground.opacity(0.08)),
```

Change the opacity to `0.12`:

```rust
                background_color: Some(colors.editor_foreground.opacity(0.12)),
```

Then add these two fields to the same `MarkdownStyle { ... }` literal (anywhere before the trailing `..Default::default()`):

```rust
            paragraph_line_height: rems(1.3).into(),
            inline_code_padding: "\u{2009}".into(),
```

(`rems(1.3)` keeps the agent/editor line height unchanged; `\u{2009}` is a thin space — the gentle agent-panel padding.)

- [ ] **Step 2: Preview "完全体" values in `with_preview_overrides`**

In `fn with_preview_overrides`, after the existing `self.inline_code.color = Some(colors.text);` line, add:

```rust
        self.paragraph_line_height = rems(1.6).into();
        self.inline_code.background_color = Some(colors.editor_foreground.opacity(0.16));
        self.inline_code_padding = "\u{2002}".into();
```

(`rems(1.6)` loosens body line height for preview only; `\u{2002}` is an en space — wider padding; deeper `0.16` background.)

- [ ] **Step 3: Build + full test**

Run: `cargo test -p markdown 2>&1 | tail -8; echo "EXIT=${PIPESTATUS[0]}"`
Expected: `test result: ok. 97 passed`, `EXIT=0` (these are construction-time value changes; the existing tests use `MarkdownStyle::default()` and are unaffected).

- [ ] **Step 4: Commit**

```bash
git add crates/markdown/src/markdown.rs
git commit -m "feat(markdown): themed values — preview line height 1.6, inline-code padding + deeper bg (agent gentle, preview full)"
```

---

## Task 5: Final verification + review

- [ ] **Step 1: Full crate test green**

Run: `cargo test -p markdown 2>&1 | tail -8; echo "EXIT=${PIPESTATUS[0]}"`
Expected: `test result: ok. 97 passed; 0 failed`, `EXIT=0`.

- [ ] **Step 2: Confirm no other crate compiled against the old field set broke**

The new `MarkdownStyle` fields have defaults and all in-tree constructors use either `..Default::default()` or `MarkdownStyle::themed*`. Spot-check callers still build:

Run: `cargo build -p markdown_preview -p agent_ui 2>&1 | tail -15; echo "EXIT=${PIPESTATUS[0]}"`
Expected: `EXIT=0`.

- [ ] **Step 3: Linus review (Rule 1)**

Run `/linus-review` on the diff (`git diff perforce-integration...markdown-readability`). Focus: data/control flow of the source-mapping change; confirm no breaking-user-space risk (selection, search highlight, copy, copy-as-markdown). Address any real finding before finishing.

- [ ] **Step 4: Report**

Summarize: tests 97/97, diff scope, any review findings + resolutions. Then hand to `superpowers:finishing-a-development-branch`.

---

## Self-Review (done at authoring)

- **Spec coverage:** preview line height (Task 2 + 4), inline-code padding both panels (Task 3 + 4), deeper bg both panels (Task 4), agent-gentle/preview-full magnitude split (Task 4), no GPUI/git changes (all edits in `crates/markdown`), TDD for the source-mapping risk (Task 3). ✓
- **Non-goals respected:** no settings exposure, no rounded corners, no fenced-block changes, no list/paragraph margin changes. ✓
- **Type consistency:** field `paragraph_line_height: DefiniteLength` set via `rems(x).into()`, read via `.line_height(self.style.paragraph_line_height)`; `inline_code_padding: SharedString` set via `"...".into()`, read via `.clone()`; helper `append_styled_no_source` used in Task 3 matches its definition. ✓
- **Placeholder scan:** none. ✓
