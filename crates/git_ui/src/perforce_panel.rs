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
use fs::RemoveOptions;
use git::perforce::{ChangelistId, ChangelistFile, PerforceChangelist, swarm_changelist_url};
use git::repository::{LogSource, RepoPath};
use git::status::{FileStatus, StageStatus};
use gpui::{
    Action, Anchor, DismissEvent, ElementId, Entity, EventEmitter, FocusHandle, Focusable,
    MouseButton, MouseDownEvent, Pixels, Point, PromptLevel, Subscription, Task,
    UniformListScrollHandle, WeakEntity, actions, anchored, deferred, rems, uniform_list,
};
use project::{
    Project,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
    project_settings::ProjectSettings,
};
use settings::Settings;
use std::ops::Range;
use util::ResultExt as _;
use ui::{ContextMenu, Scrollbars, Tab, Tooltip, WithScrollbar, prelude::*, tooltip_container};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::git_graph::open_or_reuse_graph;
use crate::git_panel::{GitPanel, GitStatusEntry};
use crate::git_panel_settings::{GitPanelScrollbarAccessor, GitPanelSettings};
use crate::git_status_icon;
use crate::project_diff::ProjectDiff;
use crate::solo_diff_view::SoloDiffView;

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
    /// Cached "is the active repo Perforce-backed", resolved asynchronously (the repository state
    /// is a lazily-polled `Shared`, so a synchronous check is unreliable). Gates the dock icon.
    active_is_perforce: bool,
    changelists: Vec<PerforceChangelist>,
    /// Flattened, collapse-aware row list driving the `uniform_list` (rebuilt on data/expand
    /// changes). Virtualized rendering keeps large changelists (hundreds of files) responsive.
    entries: Vec<RowRef>,
    /// Changelists whose files are expanded. Absent = collapsed, so the panel starts with every
    /// changelist collapsed.
    expanded: HashSet<ChangelistId>,
    /// Opened files behind head revision (`have < head`), re-fetched on each reload. Drives the
    /// out-of-date badge on file rows. Empty for non-Perforce repos.
    out_of_date: HashSet<RepoPath>,
    focus_handle: FocusHandle,
    position: DockPosition,
    /// Whether the panel is the active (visible) one in its dock. We only query `p4` while the
    /// panel is actually shown — otherwise a Perforce user with the panel closed would spawn two
    /// `p4` processes on every status change.
    active: bool,
    scroll_handle: UniformListScrollHandle,
    workspace: WeakEntity<Workspace>,
    /// Right-click context menu for a file row: the menu entity, where it was opened, and the
    /// dismiss subscription (mirrors `GitPanel`'s `context_menu`).
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
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
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        let active_repository = project.read(cx).active_repository(cx);
        let workspace_handle = workspace.weak_handle();

        cx.new(|cx| {
            // Refresh when the Zed window regains OS focus (the changelists may have changed via
            // P4V or the CLI while Zed was in the background). `reload` is internally gated on the
            // panel being the active/visible one and on a Perforce repo, so a background or
            // git-only window does no `p4` work.
            let window_activation =
                cx.observe_window_activation(window, |this: &mut Self, window, cx| {
                    if window.is_window_active() {
                        this.reload(cx);
                    }
                });
            let subscription =
                cx.subscribe(&git_store, |this: &mut Self, _git_store, event, cx| match event {
                GitStoreEvent::ActiveRepositoryChanged(_)
                | GitStoreEvent::RepositoryAdded
                | GitStoreEvent::RepositoryRemoved(_)
                | GitStoreEvent::RepositoryUpdated(
                    _,
                    RepositoryEvent::StatusesChanged | RepositoryEvent::HeadChanged,
                    true,
                ) => {
                    // Perforce detection is async; the repo may only now have resolved to a
                    // Perforce backend. Re-read the active repo and re-resolve `is_perforce`, which
                    // drives the state, caches the result, notifies the dock (so its icon appears),
                    // and reloads.
                    this.active_repository = this.project.read(cx).active_repository(cx);
                    this.refresh_is_perforce(cx);
                }
                _ => {}
            });

            let this = Self {
                project,
                active_repository,
                active_is_perforce: false,
                changelists: Vec::new(),
                entries: Vec::new(),
                expanded: HashSet::default(),
                out_of_date: HashSet::default(),
                focus_handle: cx.focus_handle(),
                position: DockPosition::Left,
                active: false,
                scroll_handle: UniformListScrollHandle::new(),
                workspace: workspace_handle,
                context_menu: None,
                reload_task: Task::ready(()),
                _subscriptions: vec![subscription, window_activation],
            };
            // Resolve `is_perforce` once at startup: the repo may already be active with no further
            // event coming, so the dock icon needs this initial async resolution to appear.
            if let Some(repo) = this.active_repository.clone() {
                let task = repo.read(cx).is_perforce_resolved(cx);
                cx.spawn(async move |this, cx| {
                    let is_perforce = task.await;
                    this.update(cx, |this, cx| {
                        this.active_is_perforce = is_perforce;
                        cx.notify();
                        this.reload(cx);
                    })
                    .ok();
                })
                .detach();
            }
            this
        })
    }

    /// Whether the active repository is Perforce-backed. Reads the cached `active_is_perforce`,
    /// which is resolved asynchronously on repository changes (see `refresh_is_perforce`) because
    /// the repository state is a lazily-polled `Shared` — a synchronous peek returns `None` until
    /// it is driven, which would keep the dock icon hidden forever.
    fn is_active_perforce(&self, _cx: &App) -> bool {
        self.active_is_perforce
    }

    /// Re-resolve whether the active repository is Perforce-backed (driving its state to
    /// completion), then cache it, notify the dock so it re-evaluates the panel icon, and reload.
    fn refresh_is_perforce(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            if self.active_is_perforce {
                self.active_is_perforce = false;
                cx.notify();
            }
            self.reload(cx);
            return;
        };
        let task = repo.read(cx).is_perforce_resolved(cx);
        cx.spawn(async move |this, cx| {
            let is_perforce = task.await;
            this.update(cx, |this, cx| {
                if this.active_is_perforce != is_perforce {
                    this.active_is_perforce = is_perforce;
                }
                // Notify unconditionally so the dock re-evaluates `icon()` even when the panel is
                // inactive (otherwise the icon can never appear: a closed panel never notifies).
                cx.notify();
                this.reload(cx);
            })
            .ok();
        })
        .detach();
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
        // Out-of-date set runs on the background executor (`p4 fstat -Ro`), so it never blocks the
        // render thread; the rows just gain their badge once it resolves.
        let stale_task = repo.read(cx).perforce_out_of_date_paths(cx);
        self.reload_task = cx.spawn(async move |this, cx| {
            let stale = stale_task.await.unwrap_or_default();
            if let Ok(groups) = task.await {
                this.update(cx, |this, cx| {
                    this.changelists = groups;
                    this.out_of_date = stale;
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

    /// The file's status icon, overlaid with a small warning triangle at the bottom-left when the
    /// file is out of date (a newer head revision exists), reusing the diagnostic-triangle style.
    fn render_file_icon(&self, file: &ChangelistFile) -> AnyElement {
        let icon = git_status_icon(file.status);
        if self.out_of_date.contains(&file.path) {
            div()
                .relative()
                .child(icon)
                .child(
                    div().absolute().bottom(px(-2.)).left(px(-3.)).child(
                        Icon::new(IconName::Triangle)
                            .size(IconSize::Indicator)
                            .color(Color::Warning),
                    ),
                )
                .into_any_element()
        } else {
            icon.into_any_element()
        }
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
        let menu_file = file.clone();

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
                    .child(self.render_file_icon(file))
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
            // Right-click opens the per-file context menu (revert / shelve / unshelve / diff /
            // swarm), gated by the file's pending-vs-shelved state.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.deploy_file_context_menu(
                        event.position,
                        menu_file.clone(),
                        source,
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }

    /// Resolve the configured Swarm changelist URL for `chnum`, or `None` when `perforce.swarm_host`
    /// is unset (the "View in Swarm" entry is then hidden).
    fn swarm_url(&self, chnum: u32, cx: &App) -> Option<String> {
        let host = ProjectSettings::get_global(cx).perforce.swarm_host.clone()?;
        let host = host.trim();
        (!host.is_empty()).then(|| swarm_changelist_url(host, chnum))
    }

    /// Store the freshly built context menu and subscribe to its dismissal (mirrors
    /// `GitPanel::set_context_menu`).
    fn set_context_menu(
        &mut self,
        context_menu: Entity<ContextMenu>,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let subscription = cx.subscribe_in(
            &context_menu,
            window,
            |this, _, _: &DismissEvent, window, cx| {
                if this.context_menu.as_ref().is_some_and(|context_menu| {
                    context_menu.0.focus_handle(cx).contains_focused(window, cx)
                }) {
                    cx.focus_self(window);
                }
                this.context_menu.take();
                cx.notify();
            },
        );
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    /// Build the right-click menu for a file row. Entries adapt to the file's state: shelve /
    /// shelve-and-revert / revert are offered for pending files (and only when their changelist is
    /// numbered, since p4 cannot shelve from the default changelist); unshelve only for shelved
    /// files; "View in Swarm" only for a numbered changelist with a configured swarm host. Open
    /// Diff is offered only for pending files (a shelved file has no working-tree diff).
    fn deploy_file_context_menu(
        &mut self,
        position: Point<Pixels>,
        file: ChangelistFile,
        source: ChangelistId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let weak = cx.weak_entity();
        let numbered = match source {
            ChangelistId::Numbered(n) => Some(n),
            ChangelistId::Default => None,
        };
        let is_pending = !file.shelved;
        let swarm = numbered.and_then(|n| self.swarm_url(n, cx));

        let context_menu = ContextMenu::build(window, cx, |menu, _window, _cx| {
            let mut menu = menu.context(self.focus_handle.clone());

            // Open the local file in the editor (applies to any file).
            {
                let f = file.clone();
                let w = weak.clone();
                menu = menu.entry("Open File", None, move |window, cx| {
                    w.update(cx, |this, cx| this.open_file(f.path.clone(), window, cx))
                        .ok();
                });
            }

            // Diff entries — only meaningful for an opened (pending) file.
            if is_pending {
                let (f1, f2) = (file.clone(), file.clone());
                let (w1, w2) = (weak.clone(), weak.clone());
                menu = menu
                    .entry("Open Diff", None, move |window, cx| {
                        w1.update(cx, |this, cx| this.open_diff(&f1, false, window, cx))
                            .ok();
                    })
                    .entry("Open Diff (File)", None, move |window, cx| {
                        w2.update(cx, |this, cx| this.open_diff(&f2, true, window, cx))
                            .ok();
                    });
            }
            // File history applies to any file (pending or shelved).
            {
                let f = file.clone();
                let w = weak.clone();
                menu = menu
                    .entry("View File History", None, move |window, cx| {
                        w.update(cx, |this, cx| {
                            this.view_file_history(f.path.clone(), window, cx)
                        })
                        .ok();
                    })
                    .separator();
            }

            // Shelve / Shelve and Revert / Revert — pending files only. Shelve requires a numbered
            // changelist.
            if is_pending {
                if let Some(chnum) = numbered {
                    let (f1, f2) = (file.clone(), file.clone());
                    let (w1, w2) = (weak.clone(), weak.clone());
                    menu = menu
                        .entry("Shelve", None, move |_window, cx| {
                            w1.update(cx, |this, cx| {
                                this.shelve_file(chnum, f1.path.clone(), false, cx)
                            })
                            .ok();
                        })
                        .entry("Shelve and Revert", None, move |_window, cx| {
                            w2.update(cx, |this, cx| {
                                this.shelve_file(chnum, f2.path.clone(), true, cx)
                            })
                            .ok();
                        });
                }
                let f = file.clone();
                let w = weak.clone();
                menu = menu.entry("Revert", None, move |window, cx| {
                    w.update(cx, |this, cx| this.revert_file(&f, window, cx)).ok();
                });
            }

            // Unshelve — shelved files only. Restores the shelf into the same changelist.
            if file.shelved {
                if let Some(chnum) = numbered {
                    let f = file.clone();
                    let w = weak.clone();
                    menu = menu.entry("Unshelve", None, move |_window, cx| {
                        w.update(cx, |this, cx| this.unshelve_file(chnum, f.path.clone(), cx))
                            .ok();
                    });
                }
            }

            // View in Swarm — needs a numbered changelist and a configured swarm host.
            if let Some(url) = swarm.clone() {
                menu = menu.separator().entry("View in Swarm", None, move |_window, cx| {
                    cx.open_url(&url);
                });
            }

            menu
        });
        self.set_context_menu(context_menu, position, window, cx);
    }

    /// Open the file's local working copy in the editor (mirrors GitPanel's "View File").
    fn open_file(&mut self, path: RepoPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            return;
        };
        let Some(project_path) = repo.read(cx).repo_path_to_project_path(&path, cx) else {
            return;
        };
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_path_preview(project_path, None, false, false, true, window, cx)
                    .detach_and_log_err(cx);
            })
            .ok();
    }

    /// Open the file's revision history in the commit-graph view (reusing git's GitGraph via
    /// `LogSource::Path`). The Perforce backend feeds it `p4 filelog` revisions.
    fn view_file_history(&mut self, path: RepoPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            return;
        };
        let repo_id = repo.read(cx).id;
        let git_store = self.project.read(cx).git_store().clone();
        self.workspace
            .update(cx, |workspace, cx| {
                open_or_reuse_graph(
                    workspace,
                    repo_id,
                    git_store,
                    LogSource::Path(path),
                    None,
                    window,
                    cx,
                );
            })
            .ok();
    }

    /// Open the file's diff. `solo` picks the single-file diff view ("Open Diff (File)"); otherwise
    /// the project diff is deployed at this file ("Open Diff"). Reuses the git diff views, which are
    /// backend-agnostic — the Perforce backend supplies the diff base via `load_committed_text`
    /// (`p4 print #have`).
    fn open_diff(
        &mut self,
        file: &ChangelistFile,
        solo: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.active_repository.clone() else {
            return;
        };
        let entry = GitStatusEntry {
            repo_path: file.path.clone(),
            status: file.status,
            staging: StageStatus::Unstaged,
            diff_stat: None,
        };
        if solo {
            SoloDiffView::open_or_focus(entry, repo, self.workspace.clone(), window, cx)
                .detach_and_log_err(cx);
        } else {
            self.workspace
                .update(cx, |workspace, cx| {
                    ProjectDiff::deploy_at(workspace, Some(entry), window, cx);
                })
                .ok();
        }
    }

    /// `p4 revert <file>`. For a file opened for *add*, p4 leaves the local file on disk, so first
    /// ask whether to also delete it (default: keep). Reloads the panel afterward.
    fn revert_file(&mut self, file: &ChangelistFile, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            return;
        };
        let path = file.path.clone();
        // Reverting an added file leaves the (now un-added) file on disk; offer to delete it.
        let delete_prompt = file.status.is_created().then(|| {
            let abs = repo.read(cx).work_directory_abs_path.join(path.as_std_path());
            let prompt = window.prompt(
                PromptLevel::Warning,
                "Revert added file?",
                Some(
                    "The file will no longer be open for add. Keep the local file on disk, or also \
                     delete it?",
                ),
                &["Keep File", "Delete File"],
                cx,
            );
            (abs, prompt)
        });
        let fs = self.project.read(cx).fs().clone();
        let revert = repo.read(cx).perforce_revert(path, cx);
        // Detached (not stored in `reload_task`): a concurrent `reload()` must not cancel the
        // delete-on-disk step or the final refresh.
        cx.spawn(async move |this, cx| {
            // Resolve the delete choice (index 1 == "Delete File"); the revert runs concurrently.
            let to_delete = if let Some((abs, prompt)) = delete_prompt {
                (prompt.await == Ok(1)).then_some(abs)
            } else {
                None
            };
            revert.await.log_err();
            if let Some(abs) = to_delete {
                fs.remove_file(
                    &abs,
                    RemoveOptions {
                        recursive: false,
                        ignore_if_not_exists: true,
                    },
                )
                .await
                .log_err();
            }
            this.update(cx, |this, cx| this.reload(cx)).ok();
        })
        .detach();
    }

    /// `p4 shelve -c <chnum> <file>`, optionally reverting afterward ("Shelve and Revert").
    /// Reloads the panel when done.
    fn shelve_file(
        &mut self,
        chnum: u32,
        path: RepoPath,
        also_revert: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.active_repository.clone() else {
            return;
        };
        let task = repo.read(cx).perforce_shelve(chnum, path, also_revert, cx);
        cx.spawn(async move |this, cx| {
            task.await.log_err();
            this.update(cx, |this, cx| this.reload(cx)).ok();
        })
        .detach();
    }

    /// `p4 unshelve -s <chnum> -c <chnum> -Af <file>` — restore the shelf into its own changelist
    /// as an opened file (the shelf stays). Reuses `perforce_move_to_changelist` with the
    /// changelist as both source and target. Reloads the panel when done.
    fn unshelve_file(&mut self, chnum: u32, path: RepoPath, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            return;
        };
        let task = repo.read(cx).perforce_move_to_changelist(
            path,
            ChangelistId::Numbered(chnum),
            Some(chnum),
            cx,
        );
        cx.spawn(async move |this, cx| {
            task.await.log_err();
            this.update(cx, |this, cx| this.reload(cx)).ok();
        })
        .detach();
    }

    /// Whether every changelist is currently expanded (drives the toolbar toggle).
    fn all_expanded(&self) -> bool {
        !self.changelists.is_empty()
            && self.changelists.iter().all(|cl| self.expanded.contains(&cl.id))
    }

    /// Toolbar toggle: collapse all changelists if all are expanded, otherwise expand all.
    fn toggle_all_expanded(&mut self, cx: &mut Context<Self>) {
        if self.all_expanded() {
            self.expanded.clear();
        } else {
            self.expanded = self.changelists.iter().map(|cl| cl.id).collect();
        }
        self.rebuild_entries();
        cx.notify();
    }

    /// The toolbar above the changelist list: a single collapse-all / expand-all toggle, styled
    /// like the git panel's header strip.
    fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let all_expanded = self.all_expanded();
        let (icon, tooltip) = if all_expanded {
            (IconName::ListCollapse, "Collapse All")
        } else {
            (IconName::ListTree, "Expand All")
        };
        h_flex()
            .h(Tab::container_height(cx))
            .w_full()
            .flex_none()
            .px_1()
            .justify_end()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                IconButton::new("perforce-toggle-all", icon)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text(tooltip))
                    .on_click(cx.listener(|this, _, _window, cx| this.toggle_all_expanded(cx))),
            )
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
        base.child(self.render_toolbar(cx))
            .child(
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
        .children(self.context_menu.as_ref().map(|(menu, position, _)| {
            deferred(
                anchored()
                    .position(*position)
                    .anchor(Anchor::TopLeft)
                    .child(menu.clone()),
            )
            .with_priority(1)
        }))
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
