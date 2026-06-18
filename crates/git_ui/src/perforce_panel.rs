//! Perforce Changes panel — a multi-changelist view of the active Perforce workspace.
//!
//! Deliberately a **separate** panel from [`crate::git_panel::GitPanel`] rather than an extension
//! of it: the git panel is built around git staging (a fixed Conflict/Tracked/New section model
//! with per-file staged/unstaged toggles and an index), concepts Perforce does not have. Bolting
//! dynamic changelist grouping into it would rewrite a core panel every git user depends on. This
//! panel instead consumes the Perforce-only [`Repository::perforce_changelists`] and renders with
//! the same Zed design components (`ListItem`, status icons, badges). Its dock button only appears
//! when the active repository is Perforce-backed, so git workspaces are completely unaffected.

use collections::HashSet;
use git::perforce::{ChangelistId, ChangelistFile, PerforceChangelist};
use git::repository::RepoPath;
use git::status::FileStatus;
use gpui::{
    Action, ElementId, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
    UniformListScrollHandle, WeakEntity, actions, rems, uniform_list,
};
use project::{
    Project,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
};
use settings::Settings;
use std::ops::Range;
use util::ResultExt as _;
use ui::{Scrollbars, WithScrollbar, prelude::*, tooltip_container};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::git_panel::GitPanel;
use crate::git_panel_settings::{GitPanelScrollbarAccessor, GitPanelSettings};
use crate::git_status_icon;

actions!(
    perforce_panel,
    [
        /// Toggles focus on the Perforce Changes panel.
        ToggleFocus,
    ]
);

const PERFORCE_PANEL_KEY: &str = "PerforcePanel";

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<PerforcePanel>(window, cx);
    });
}

/// A single virtualized row: either a changelist header or a file under an expanded changelist.
/// Holds indices into [`PerforcePanel::changelists`] so the `uniform_list` render closure can look
/// up the data lazily (matching `GitPanel`'s flat-entry model).
#[derive(Clone, Copy)]
enum RowRef {
    Header(usize),
    File(usize, usize),
}

/// Drag-and-drop payload: a file being dragged from its changelist onto another. `shelved`
/// selects the drop behavior (pending → `p4 reopen`, shelved → `p4 unshelve`); `source` is the
/// changelist it currently lives in (the shelf source for unshelve).
#[derive(Clone)]
struct DraggedPerforceFile {
    path: RepoPath,
    source: ChangelistId,
    shelved: bool,
    name: SharedString,
    status: FileStatus,
}

/// The chip that follows the cursor while dragging a file. Matches the file row (status icon +
/// filename at the same font size), minus the path and shelved mark.
struct DraggedPerforceFileView {
    name: SharedString,
    status: FileStatus,
}

impl Render for DraggedPerforceFileView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_2()
            .py_0p5()
            .gap_1()
            .rounded_sm()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(git_status_icon(self.status))
            .child(Label::new(self.name.clone()))
    }
}

pub struct PerforcePanel {
    project: Entity<Project>,
    active_repository: Option<Entity<Repository>>,
    changelists: Vec<PerforceChangelist>,
    /// Flattened, collapse-aware row list driving the `uniform_list` (rebuilt on data/expand
    /// changes). Virtualized rendering keeps large changelists (hundreds of files) responsive.
    entries: Vec<RowRef>,
    /// Changelists whose files are expanded. Absent = collapsed, so the panel starts with every
    /// changelist collapsed.
    expanded: HashSet<ChangelistId>,
    focus_handle: FocusHandle,
    position: DockPosition,
    /// Whether the panel is the active (visible) one in its dock. We only query `p4` while the
    /// panel is actually shown — otherwise a Perforce user with the panel closed would spawn two
    /// `p4` processes on every status change.
    active: bool,
    scroll_handle: UniformListScrollHandle,
    workspace: WeakEntity<Workspace>,
    reload_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl PerforcePanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: gpui::AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            PerforcePanel::new(workspace, window, cx)
        })
    }

    fn new(
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        let active_repository = project.read(cx).active_repository(cx);
        let workspace_handle = workspace.weak_handle();

        cx.new(|cx| {
            let subscription =
                cx.subscribe(&git_store, |this: &mut Self, _git_store, event, cx| match event {
                GitStoreEvent::ActiveRepositoryChanged(_)
                | GitStoreEvent::RepositoryAdded
                | GitStoreEvent::RepositoryRemoved(_) => {
                    this.active_repository = this.project.read(cx).active_repository(cx);
                    this.reload(cx);
                }
                GitStoreEvent::RepositoryUpdated(
                    _,
                    RepositoryEvent::StatusesChanged | RepositoryEvent::HeadChanged,
                    true,
                ) => {
                    this.reload(cx);
                }
                _ => {}
            });

            let mut this = Self {
                project,
                active_repository,
                changelists: Vec::new(),
                entries: Vec::new(),
                expanded: HashSet::default(),
                focus_handle: cx.focus_handle(),
                position: DockPosition::Left,
                active: false,
                scroll_handle: UniformListScrollHandle::new(),
                workspace: workspace_handle,
                reload_task: Task::ready(()),
                _subscriptions: vec![subscription],
            };
            // Initial population happens on `set_active(true)` when the panel is first shown;
            // while hidden we intentionally issue no `p4` queries.
            this
        })
    }

    fn is_active_perforce(&self, cx: &App) -> bool {
        self.active_repository
            .as_ref()
            .is_some_and(|repo| repo.read(cx).is_perforce())
    }

    /// Re-fetch the pending changelists for the active Perforce repository. A no-op (clears the
    /// view) for git / non-Perforce repositories, and skipped entirely while the panel is hidden
    /// so a closed panel never spawns `p4` processes on status churn.
    fn reload(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        if !self.is_active_perforce(cx) {
            if !self.changelists.is_empty() {
                self.changelists.clear();
                self.rebuild_entries();
                cx.notify();
            }
            return;
        }
        let Some(repo) = self.active_repository.clone() else {
            return;
        };
        let task = repo.read(cx).perforce_changelists(cx);
        self.reload_task = cx.spawn(async move |this, cx| {
            if let Ok(groups) = task.await {
                this.update(cx, |this, cx| {
                    this.changelists = groups;
                    this.rebuild_entries();
                    cx.notify();
                })
                .ok();
            }
        });
    }

    /// Rebuild the flat row list from the current changelists + expand state. Collapsed
    /// changelists contribute only their header row.
    fn rebuild_entries(&mut self) {
        self.entries.clear();
        for (ci, cl) in self.changelists.iter().enumerate() {
            self.entries.push(RowRef::Header(ci));
            if self.expanded.contains(&cl.id) {
                for fi in 0..cl.files.len() {
                    self.entries.push(RowRef::File(ci, fi));
                }
            }
        }
    }

    /// Row height shared by changelist headers and file rows — matches `GitPanel::list_item_height`
    /// so the two SCM panels feel identical.
    fn list_item_height(&self) -> Rems {
        rems(1.75)
    }

    fn toggle_expanded(&mut self, id: ChangelistId, cx: &mut Context<Self>) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        self.rebuild_entries();
        cx.notify();
    }

    /// Handle a file dropped onto the `target` changelist header. Pending files are `reopen`ed;
    /// shelved files are `unshelve`d into the target (see `perforce_move_to_changelist`). After the
    /// p4 command completes, the panel reloads so the move is reflected.
    fn drop_on_changelist(
        &mut self,
        dragged: &DraggedPerforceFile,
        target: ChangelistId,
        cx: &mut Context<Self>,
    ) {
        // A pending file dropped on the changelist it already lives in: nothing to do.
        if !dragged.shelved && dragged.source == target {
            return;
        }
        let Some(repo) = self.active_repository.clone() else {
            return;
        };
        let shelved_source = match (dragged.shelved, dragged.source) {
            (true, ChangelistId::Numbered(n)) => Some(n),
            _ => None,
        };
        let task = repo.read(cx).perforce_move_to_changelist(
            dragged.path.clone(),
            target,
            shelved_source,
            cx,
        );
        cx.spawn(async move |this, cx| {
            task.await.log_err();
            this.update(cx, |this, cx| this.reload(cx)).ok();
        })
        .detach();
    }

    fn render_changelist_header(
        &self,
        changelist: &PerforceChangelist,
        cx: &Context<Self>,
    ) -> AnyElement {
        let id = changelist.id;
        let expanded = self.expanded.contains(&id);
        // Inline header uses only the first description line (the rest would fight the single-row
        // truncation); the full multi-line description lives in the hover tooltip.
        let first_line = changelist.description.lines().next().unwrap_or_default();
        let title = match id {
            ChangelistId::Default => "Default Changelist".to_string(),
            ChangelistId::Numbered(n) if first_line.is_empty() => format!("#{n}"),
            ChangelistId::Numbered(n) => format!("#{n}: {first_line}"),
        };
        // Split the header counter into pending+shelved, always showing both numbers (incl. 0) so
        // the pending/shelved breakdown is unambiguous.
        let pending_count = changelist.files.iter().filter(|f| !f.shelved).count();
        let shelved_count = changelist.files.iter().filter(|f| f.shelved).count();
        let element_id: ElementId = ElementId::Name(format!("cl_{id:?}").into());
        // Full tooltip text: the changelist number then its complete (possibly multi-line)
        // description, for a documentation-style preview. None for the default changelist.
        let tooltip_text: Option<SharedString> = match id {
            ChangelistId::Default => None,
            ChangelistId::Numbered(_) if changelist.description.is_empty() => None,
            ChangelistId::Numbered(n) => Some(format!("#{n}\n{}", changelist.description).into()),
        };

        h_flex()
            .id(element_id)
            .group("cl-header")
            .cursor_pointer()
            .h(self.list_item_height())
            .w_full()
            .pl_3()
            .pr_1()
            .gap_1p5()
            .justify_between()
            .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
            .child(
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .child(
                // `pending+shelved`, shelved count tinted like the per-file "shelved" badge.
                h_flex()
                    .flex_none()
                    .child(
                        Label::new(format!("{pending_count}+"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(shelved_count.to_string())
                            .size(LabelSize::Small)
                            .color(if shelved_count > 0 {
                                Color::Accent
                            } else {
                                Color::Muted
                            }),
                    ),
            )
            .when_some(tooltip_text, |this, text| {
                this.tooltip(move |_window, cx| {
                    cx.new(|_| ChangelistTooltip { text: text.clone() }).into()
                })
            })
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.toggle_expanded(id, cx);
            }))
            // Drop target: a file dragged onto this header moves into this changelist.
            .drag_over::<DraggedPerforceFile>(|style, _dragged, _window, cx| {
                style.bg(cx.theme().colors().drop_target_background)
            })
            .on_drop(cx.listener(move |this, dragged: &DraggedPerforceFile, _window, cx| {
                this.drop_on_changelist(dragged, id, cx);
            }))
            .into_any_element()
    }

    fn render_file_row(
        &self,
        file: &ChangelistFile,
        source: ChangelistId,
        cx: &Context<Self>,
    ) -> AnyElement {
        let unix = file.path.as_unix_str();
        let (dir, name) = match unix.rsplit_once('/') {
            Some((dir, name)) => (Some(format!("{dir}/")), name.to_string()),
            None => (None, unix.to_string()),
        };
        let path_color = if file.status.is_deleted() {
            Color::Disabled
        } else {
            Color::Muted
        };

        h_flex()
            // A stable id makes the row interactive so gpui repaints its hover highlight on
            // mouse-move (like GitPanel's rows). Without it the highlight only updated on scroll.
            // A file can appear both opened and shelved in one changelist, so the id is keyed by
            // path *and* the shelved flag to stay unique.
            .id(ElementId::Name(
                format!(
                    "p4_file_{}_{}",
                    if file.shelved { "s" } else { "p" },
                    file.path.as_unix_str()
                )
                .into(),
            ))
            .h(self.list_item_height())
            .w_full()
            .pl_3()
            .pr_1()
            .gap_1p5()
            .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
            // Drag source: relocate this file into another changelist by dropping on its header.
            .on_drag(
                DraggedPerforceFile {
                    path: file.path.clone(),
                    source,
                    shelved: file.shelved,
                    name: name.clone().into(),
                    status: file.status,
                },
                |dragged, _offset, _window, cx| {
                    cx.new(|_| DraggedPerforceFileView {
                        name: dragged.name.clone(),
                        status: dragged.status,
                    })
                },
            )
            .child(
                // Mirror GitPanel's name row: filename never truncates, parent dir truncates from
                // the start (so the meaningful tail stays visible), same gaps and label sizes.
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .child(git_status_icon(file.status))
                    .child(
                        h_flex()
                            .min_w_0()
                            .overflow_hidden()
                            .child(
                                div()
                                    .flex_none()
                                    .child(Label::new(format!("{name} "))),
                            )
                            .when_some(dir, |this, dir| {
                                this.child(Label::new(dir).color(path_color).truncate_start())
                            }),
                    ),
            )
            .when(file.shelved, |row| {
                row.child(
                    Label::new("shelved")
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
            })
            .into_any_element()
    }
}

impl Focusable for PerforcePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for PerforcePanel {}

impl Render for PerforcePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let base = v_flex()
            .key_context("PerforcePanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .bg(cx.theme().colors().panel_background);

        if self.entries.is_empty() {
            return base.child(
                h_flex().p_4().child(
                    Label::new("No pending changelists")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            );
        }

        let entry_count = self.entries.len();
        base.child(
            // Parent of the virtualized list (like GitPanel): holds the `uniform_list` and the
            // themed scrollbar overlay, both wired to the shared scroll handle. Virtualization
            // keeps hundreds of files responsive.
            h_flex()
                .size_full()
                .overflow_hidden()
                .child(
                    uniform_list(
                        "perforce-entries",
                        entry_count,
                        cx.processor(|this, range: Range<usize>, _window, cx| {
                            let mut items = Vec::with_capacity(range.end - range.start);
                            for ix in range {
                                match this.entries[ix] {
                                    RowRef::Header(ci) => {
                                        items.push(
                                            this.render_changelist_header(&this.changelists[ci], cx),
                                        );
                                    }
                                    RowRef::File(ci, fi) => {
                                        let cl = &this.changelists[ci];
                                        items.push(this.render_file_row(
                                            &cl.files[fi],
                                            cl.id,
                                            cx,
                                        ));
                                    }
                                }
                            }
                            items
                        }),
                    )
                    .size_full()
                    .flex_grow_1()
                    .track_scroll(&self.scroll_handle),
                )
                .custom_scrollbars(
                    Scrollbars::for_settings::<GitPanelScrollbarAccessor>()
                        .tracked_scroll_handle(&self.scroll_handle),
                    window,
                    cx,
                ),
        )
    }
}

/// A wide, multi-line hover preview of a changelist's full description (documentation-style),
/// since the default tooltip is single-line and narrow. Each source line becomes its own row
/// (blank lines preserved as small gaps) so paragraph structure survives.
struct ChangelistTooltip {
    text: SharedString,
}

impl Render for ChangelistTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lines: Vec<String> = self.text.split('\n').map(str::to_string).collect();
        tooltip_container(cx, move |el, _| {
            el.child(
                // FIXED width (like `commit_tooltip`'s `w(rems(30.))`), not `max_w`: a max-width
                // doesn't constrain the measure pass, so wrapping lines get measured at one row
                // but painted at several — the background then ends short of the text. A fixed
                // width makes measure and paint wrap identically, so the height is correct.
                v_flex().w(px(440.)).children(lines.into_iter().map(|line| {
                    if line.is_empty() {
                        div().h(px(6.)).into_any_element()
                    } else {
                        div().child(line).into_any_element()
                    }
                })),
            )
        })
    }
}

impl Panel for PerforcePanel {
    fn persistent_name() -> &'static str {
        "PerforcePanel"
    }

    fn panel_key() -> &'static str {
        PERFORCE_PANEL_KEY
    }

    fn position(&self, _: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        // Refresh on becoming visible so the user sees current changelists immediately.
        if active {
            self.reload(cx);
        }
    }

    fn default_size(&self, window: &Window, cx: &App) -> Pixels {
        // Mirror the git panel's *effective* width (its persisted size, or its default) so the
        // Perforce panel lines up exactly with its SCM sibling on first open — not just with the
        // git default, which the user may have resized away from.
        self.workspace
            .upgrade()
            .and_then(|ws| {
                let ws = ws.read(cx);
                let git_panel = ws.panel::<GitPanel>(cx)?;
                ws.dock_at_position(self.position)
                    .read(cx)
                    .stored_panel_size(&git_panel, window, cx)
            })
            .unwrap_or_else(|| GitPanelSettings::get_global(cx).default_width)
    }

    fn icon(&self, _: &Window, cx: &App) -> Option<ui::IconName> {
        // Only surface the dock button in a Perforce workspace, so git users never see it.
        self.is_active_perforce(cx)
            .then_some(ui::IconName::ListTree)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Perforce Changes")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Sits between the git panel (3) and the collab panel (5) in the dock button strip.
        4
    }
}
