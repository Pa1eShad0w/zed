//! Scheduled messages: persisted data types, fire-time text parsing, the
//! per-session in-memory set, garbage collection, the global store that
//! owns persistence, and the UI (send-button dropdown, custom-time popover,
//! scheduled block).
//!
//! See docs/scheduled-messages.spec.md (§2 data structures, §3 persistence,
//! §4.3 GC, §5 time grammar, §6 UX). Everything except
//! [`ScheduledMessageStore`] and the UI is pure logic, unit testable without
//! an app context.

use acp_thread::ThreadStatus;
use agent_client_protocol::schema::v1 as acp;
use chrono::{DateTime, Local, TimeZone as _, Utc};
use collections::HashMap;
use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent, EditorMode};
use futures::FutureExt as _;
use futures::future::Shared;
use gpui::{
    AnyElement, App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, Global, Subscription, Task, TaskExt as _, UpdateGlobal as _, WeakEntity, Window,
};
use project::AgentId;
use serde::{Deserialize, Serialize};
use ui::{
    ButtonLike, ContextMenu, ContextMenuEntry, ElevationIndex, PopoverMenu, PopoverMenuHandle,
    TintColor, Tooltip, prelude::*,
};
use util::ResultExt as _;

use super::ThreadView;
use crate::message_editor::MessageEditor;

/// Stable identity of a scheduled message. Unlike `QueueEntryId` (a
/// process-local counter), this survives Zed restarts because entries are
/// persisted, so it is a random UUID string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScheduledMessageId(String);

impl ScheduledMessageId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for ScheduledMessageId {
    fn default() -> Self {
        Self::new()
    }
}

/// A message waiting to be sent at `fire_at`. Buffer tracking state is not
/// persisted (GPUI entities are not serializable); firing passes an empty
/// tracked-buffer set, which only affects stale-buffer detection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledMessage {
    pub id: ScheduledMessageId,
    pub content: Vec<acp::ContentBlock>,
    pub fire_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

pub const SCHEDULED_MESSAGES_FILE_VERSION: u32 = 1;

/// Persisted form of all scheduled messages across sessions, stored as a
/// single JSON value under one KV key. Session keys have the form
/// `"{agent_id}|{session_id}"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledMessagesFile {
    pub version: u32,
    pub sessions: HashMap<String, Vec<ScheduledMessage>>,
}

/// How long an entry may stay overdue before it is considered abandoned.
/// See docs/scheduled-messages.spec.md §4.3.
const EXPIRY_DAYS: i64 = 30;

/// Drops entries whose fire time passed more than thirty days ago, then drops
/// sessions left empty. Run once when the store loads the persisted file, to
/// reap entries orphaned by sessions that can never be resumed.
///
/// The TTL keys off `fire_at`, not `created_at`: a message scheduled far in
/// advance stays valid no matter how long ago it was created, and an overdue
/// entry within the window still catch-up fires when its thread reopens.
pub fn gc(file: ScheduledMessagesFile, now: DateTime<Utc>) -> ScheduledMessagesFile {
    let cutoff = now - chrono::Duration::days(EXPIRY_DAYS);
    let sessions = file
        .sessions
        .into_iter()
        .filter_map(|(session_key, messages)| {
            let kept: Vec<_> = messages
                .into_iter()
                .filter(|message| message.fire_at >= cutoff)
                .collect();
            (!kept.is_empty()).then_some((session_key, kept))
        })
        .collect();
    ScheduledMessagesFile {
        version: file.version,
        sessions,
    }
}

/// KV namespace for scheduled messages (spec §3). It holds exactly one key:
/// `ScopedKeyValueStore` cannot enumerate keys, so per-session keys would
/// leave unreachable orphan records once their session is forgotten.
const NAMESPACE: &str = "scheduled-messages";
const FILE_KEY: &str = "all";

/// Single owner of every scheduled message across sessions (spec §2.2).
/// Loaded once at startup and held in memory; every mutation serializes the
/// whole file back to the single KV key as a fire-and-forget background
/// write. The in-memory state is authoritative for the session.
///
/// Registered as a GPUI global by [`ScheduledMessageStore::init`]; access it
/// through the `gpui::ReadGlobal` / `gpui::UpdateGlobal` traits, e.g.
/// `ScheduledMessageStore::global(cx)` and
/// `ScheduledMessageStore::update_global(cx, |store, cx| ...)`.
pub struct ScheduledMessageStore {
    file: ScheduledMessagesFile,
    // Chains background writes so they land in mutation order. Every write
    // overwrites the same key, so two unordered writes could otherwise
    // finish last-spawned-first and persist stale state. Shared so `remove`
    // can hand callers a clone to await (see `remove` docs).
    pending_write: Option<Shared<Task<()>>>,
}

impl Global for ScheduledMessageStore {}

impl ScheduledMessageStore {
    /// Synchronously reads the persisted file, drops expired entries
    /// (spec §4.3), and registers the store as a global. Total volume is a
    /// handful of messages, so the synchronous read is effectively free.
    pub fn init(cx: &mut App) {
        let store = Self::load(cx);
        cx.set_global(store);
    }

    fn load(cx: &App) -> Self {
        let raw = KeyValueStore::global(cx)
            .scoped(NAMESPACE)
            .read(FILE_KEY)
            .log_err()
            .flatten();
        // A persisted value that fails to deserialize (e.g. the content
        // block schema changed across an upstream uptake) or carries an
        // unknown version must never break startup: warn and start empty.
        // Version 1 has nothing to migrate (spec §3).
        let file = raw
            .and_then(
                |raw| match serde_json::from_str::<ScheduledMessagesFile>(&raw) {
                    Ok(file) if file.version == SCHEDULED_MESSAGES_FILE_VERSION => Some(file),
                    Ok(file) => {
                        log::warn!(
                            "dropping scheduled messages with unsupported version {}",
                            file.version
                        );
                        None
                    }
                    Err(error) => {
                        log::warn!(
                            "dropping persisted scheduled messages that no longer \
                             deserialize: {error}"
                        );
                        None
                    }
                },
            )
            .unwrap_or_else(|| ScheduledMessagesFile {
                version: SCHEDULED_MESSAGES_FILE_VERSION,
                sessions: HashMap::default(),
            });
        let mut store = Self {
            file: gc(file.clone(), Utc::now()),
            pending_write: None,
        };
        // Write back only when GC actually dropped something, so a normal
        // start doesn't touch the database.
        if store.file != file {
            store.persist(cx);
        }
        store
    }

    /// This session's messages, cloned out. Session keys have the form
    /// `"{agent_id}|{session_id}"`.
    pub fn messages_for(&self, session_key: &str) -> Vec<ScheduledMessage> {
        self.file
            .sessions
            .get(session_key)
            .cloned()
            .unwrap_or_default()
    }

    pub fn add(&mut self, session_key: String, message: ScheduledMessage, cx: &mut App) {
        self.file
            .sessions
            .entry(session_key)
            .or_default()
            .push(message);
        self.persist(cx);
    }

    /// Removes one message; a session left with no messages is dropped
    /// entirely so the persisted file doesn't accumulate dead session keys.
    ///
    /// Returns the removed message together with a task that resolves once
    /// this deletion (including every write chained before it) has been
    /// persisted. The fire path must await that task before enqueueing the
    /// content: if Zed crashed between the send and the write, the record
    /// would resurrect on restart as an overdue catch-up and double-send
    /// (spec §3 prefers loss over double-send).
    pub fn remove(
        &mut self,
        session_key: &str,
        id: &ScheduledMessageId,
        cx: &mut App,
    ) -> Option<(ScheduledMessage, Shared<Task<()>>)> {
        let messages = self.file.sessions.get_mut(session_key)?;
        let index = messages.iter().position(|message| &message.id == id)?;
        let removed = messages.remove(index);
        if messages.is_empty() {
            self.file.sessions.remove(session_key);
        }
        self.persist(cx);
        // `persist` only fails to queue a write when serialization fails; a
        // ready task keeps the caller's await from hanging in that case.
        let persisted = self
            .pending_write
            .clone()
            .unwrap_or_else(|| Task::ready(()).shared());
        Some((removed, persisted))
    }

    /// Serializes the whole file and overwrites the single key in the
    /// background. Failures are logged, not surfaced: the in-memory store
    /// stays authoritative for the rest of the session.
    fn persist(&mut self, cx: &App) {
        let Some(payload) = serde_json::to_string(&self.file).log_err() else {
            return;
        };
        let kvp = KeyValueStore::global(cx);
        let previous_write = self.pending_write.take();
        self.pending_write = Some(
            cx.background_spawn(async move {
                if let Some(previous_write) = previous_write {
                    previous_write.await;
                }
                kvp.scoped(NAMESPACE)
                    .write(FILE_KEY.to_string(), payload)
                    .await
                    .log_err();
            })
            .shared(),
        );
    }
}

/// One session's scheduled messages, kept sorted by fire time ascending
/// (ties broken by creation time). Pure data — the owning view drives timers
/// and rendering from it.
#[derive(Debug, Default)]
pub struct ScheduledMessageSet {
    // Invariant: sorted by (fire_at, created_at) ascending. All insertion
    // goes through `add`, which keeps the order, so iteration and
    // `due_entries` never need to sort.
    entries: Vec<ScheduledMessage>,
}

impl ScheduledMessageSet {
    pub fn add(&mut self, message: ScheduledMessage) {
        // Ids are freshly generated UUIDs or restored from unique persisted
        // records, so duplicates indicate a caller bug: `remove`/`get` would
        // silently hit only the first copy.
        debug_assert!(
            self.get(&message.id).is_none(),
            "scheduled message ids must be unique within a session"
        );
        let index = self.entries.partition_point(|entry| {
            (entry.fire_at, entry.created_at) <= (message.fire_at, message.created_at)
        });
        self.entries.insert(index, message);
    }

    pub fn remove(&mut self, id: &ScheduledMessageId) -> Option<ScheduledMessage> {
        let index = self.entries.iter().position(|entry| &entry.id == id)?;
        Some(self.entries.remove(index))
    }

    pub fn get(&self, id: &ScheduledMessageId) -> Option<&ScheduledMessage> {
        self.entries.iter().find(|entry| &entry.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ScheduledMessage> {
        self.entries.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// IDs of every entry whose fire time has arrived (`fire_at <= now`),
    /// in fire order. Firing them in this order preserves FIFO semantics in
    /// the message queue they are handed to.
    // Production uses the grace-aware `ScheduledMessagesState::due_now`;
    // this raw variant is pinned by the set's unit tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn due_entries(&self, now: DateTime<Utc>) -> Vec<ScheduledMessageId> {
        self.entries
            .iter()
            .take_while(|entry| entry.fire_at <= now)
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// The earliest fire time.
    // Production re-arms the timer from the grace-aware
    // `ScheduledMessagesState::next_wake`; this raw variant is pinned by
    // the set's unit tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn next_fire_at(&self) -> Option<DateTime<Utc>> {
        self.entries.first().map(|entry| entry.fire_at)
    }
}

/// How long restored-overdue entries are held before they catch-up fire,
/// giving the user a chance to cancel a stale send (spec §4.2).
const OVERDUE_GRACE_SECONDS: i64 = 10;

/// The grace window for entries whose fire time had already passed when
/// their thread view was constructed. All such entries share one deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverdueGrace {
    /// The view-construction instant: entries with `fire_at <= cutoff` are
    /// the restored-overdue ones held by this window. Entries scheduled
    /// later always have a future fire time, so they never match.
    pub cutoff: DateTime<Utc>,
    /// When the held entries actually fire (`cutoff` + ten seconds).
    pub deadline: DateTime<Utc>,
}

/// Runtime state for one session's scheduled messages, owned by its thread
/// view. The set mirrors this session's slice of [`ScheduledMessageStore`];
/// the editors are read-only [`MessageEditor`]s used to render entry
/// content (spec §2.1).
#[derive(Default)]
pub struct ScheduledMessagesState {
    /// Store key of the owning session, `"{agent_id}|{session_id}"`.
    pub session_key: String,
    pub set: ScheduledMessageSet,
    pub editors: HashMap<ScheduledMessageId, Entity<MessageEditor>>,
    pub overdue_grace: Option<OverdueGrace>,
    /// Sleeps until [`Self::next_wake`], then runs the fire routine.
    /// Re-armed after every mutation of the set.
    pub timer: Option<Task<()>>,
    /// Notifies the view once a second so entry countdowns stay live.
    /// Running only while the set is non-empty.
    pub countdown_ticker: Option<Task<()>>,
    /// The send-button dropdown with the preset fire times (spec §6.1).
    pub schedule_menu_handle: PopoverMenuHandle<ContextMenu>,
    /// The custom-time input popover, shown from the dropdown's
    /// "Custom time…" item (spec §6.1).
    pub time_picker_handle: PopoverMenuHandle<ScheduleTimePicker>,
}

impl ScheduledMessagesState {
    /// When this entry should actually fire: grace-held entries resolve to
    /// the shared grace deadline, everything else to its own fire time.
    pub fn effective_fire_at(&self, message: &ScheduledMessage) -> DateTime<Utc> {
        match self.overdue_grace {
            Some(grace) if message.fire_at <= grace.cutoff => grace.deadline,
            _ => message.fire_at,
        }
    }

    /// IDs of entries due at `now`, in fire order. Uses effective fire
    /// times, so grace-held entries stay out until the deadline passes.
    pub fn due_now(&self, now: DateTime<Utc>) -> Vec<ScheduledMessageId> {
        self.set
            .iter()
            .filter(|message| self.effective_fire_at(message) <= now)
            .map(|message| message.id.clone())
            .collect()
    }

    /// The earliest instant anything should fire; `None` when the set is
    /// empty (so the timer stands down).
    pub fn next_wake(&self) -> Option<DateTime<Utc>> {
        self.set
            .iter()
            .map(|message| self.effective_fire_at(message))
            .min()
    }

    /// Drops one entry and its display editor. When the last entry held by
    /// the overdue grace window leaves the set — fired, withdrawn, or
    /// deleted — the window is cleared, so stale grace state can never
    /// outlive the entries it applied to.
    pub fn remove_entry(&mut self, id: &ScheduledMessageId) -> Option<ScheduledMessage> {
        self.editors.remove(id);
        let removed = self.set.remove(id)?;
        if let Some(grace) = self.overdue_grace
            && !self
                .set
                .iter()
                .any(|message| message.fire_at <= grace.cutoff)
        {
            self.overdue_grace = None;
        }
        Some(removed)
    }
}

/// The store key for one thread's scheduled messages (spec §2.2).
pub fn session_key(agent_id: &AgentId, session_id: &acp::SessionId) -> String {
    format!("{agent_id}|{session_id}")
}

/// Scheduled-message behavior of the thread view. The bodies live here
/// rather than in `thread_view.rs` to keep that file's footprint at the
/// field declaration plus one constructor call; same-crate field visibility
/// makes this split free.
impl ThreadView {
    /// Rebuilds this session's runtime state from the global store: one
    /// read-only display editor per entry, overdue entries (fire time
    /// already past) held in a shared ten-second grace window instead of
    /// firing instantly, and the wake-up timer armed. Called once from the
    /// constructor (spec §4.2).
    pub(crate) fn restore_scheduled_messages(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scheduled.session_key = session_key(&self.agent_id, &self.session_id);
        // Absent only in tests that never initialize the store; production
        // registers it at startup.
        let Some(store) = cx.try_global::<ScheduledMessageStore>() else {
            return;
        };
        let messages = store.messages_for(&self.scheduled.session_key);
        if messages.is_empty() {
            return;
        }
        let now = Utc::now();
        if messages.iter().any(|message| message.fire_at <= now) {
            self.scheduled.overdue_grace = Some(OverdueGrace {
                cutoff: now,
                deadline: now + chrono::Duration::seconds(OVERDUE_GRACE_SECONDS),
            });
        }
        for message in messages {
            let editor = self.build_scheduled_display_editor(message.content.clone(), window, cx);
            self.scheduled.editors.insert(message.id.clone(), editor);
            self.scheduled.set.add(message);
        }
        self.arm_scheduled_message_timer(window, cx);
    }

    /// A read-only editor rendering one scheduled entry's content, built the
    /// same way as the queued-message display editors.
    fn build_scheduled_display_editor(
        &self,
        content: Vec<acp::ContentBlock>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<MessageEditor> {
        cx.new(|cx| {
            let mut editor = MessageEditor::new(
                self.workspace.clone(),
                self.project.clone(),
                None,
                self.session_capabilities.clone(),
                self.agent_id.clone(),
                "",
                EditorMode::AutoHeight {
                    min_lines: 1,
                    max_lines: Some(10),
                },
                window,
                cx,
            );
            editor.set_read_only(true, cx);
            editor.set_message(content, window, cx);
            editor
        })
    }

    /// (Re)arms the wake-up: sleeps until the next effective fire time, then
    /// runs the fire routine. The routine re-checks the wall clock, so an
    /// early wake merely re-arms with the remaining delay. Replacing the
    /// task cancels any previous timer (spec §4.1).
    ///
    /// Also manages the one-second countdown ticker for the scheduled block:
    /// every set mutation funnels through here, so this is the single place
    /// that can start it on the first entry and stop it on the last.
    pub(crate) fn arm_scheduled_message_timer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_scheduled_countdown_ticker(cx);
        let Some(wake_at) = self.scheduled.next_wake() else {
            self.scheduled.timer = None;
            return;
        };
        let delay = (wake_at - Utc::now()).to_std().unwrap_or_default();
        self.scheduled.timer = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update_in(cx, |this, window, cx| {
                this.fire_due_scheduled_messages(window, cx);
            })
            .ok();
        }));
    }

    /// Keeps the scheduled block's countdowns live: while entries exist a
    /// task notifies the view once a second; when the set empties the task
    /// is dropped so an idle view costs nothing.
    fn update_scheduled_countdown_ticker(&mut self, cx: &mut Context<Self>) {
        if self.scheduled.set.is_empty() {
            self.scheduled.countdown_ticker = None;
        } else if self.scheduled.countdown_ticker.is_none() {
            self.scheduled.countdown_ticker = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(1))
                        .await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        }
    }

    /// Fires every entry whose effective fire time has arrived, oldest
    /// first (spec §4.1).
    pub(crate) fn fire_due_scheduled_messages(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let due = self.scheduled.due_now(Utc::now());
        self.fire_scheduled_messages(due, window, cx);
    }

    /// The shared fire routine, driven by the wake-up timer (everything
    /// due) and by "Send Now" (one entry). For each entry the store record
    /// is deleted and that deletion is awaited to disk *before* the content
    /// is enqueued: a crash after the send must not resurrect the record on
    /// restart and double-send it (spec §3 prefers loss over double-send).
    /// After enqueueing, an idle thread dispatches the queue front through
    /// the existing send-now path, which yields all three behaviors of the
    /// spec §4 table: generating → the fired message waits at the queue
    /// tail; idle with an empty queue → the fired message itself starts a
    /// turn; idle with a stale paused queue → the queue front drains first.
    fn fire_scheduled_messages(
        &mut self,
        ids: Vec<ScheduledMessageId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut fired = Vec::new();
        let store_exists = cx.try_global::<ScheduledMessageStore>().is_some();
        for id in ids {
            let Some(message) = self.scheduled.remove_entry(&id) else {
                continue;
            };
            let persisted = if store_exists {
                ScheduledMessageStore::update_global(cx, |store, cx| {
                    store.remove(&self.scheduled.session_key, &id, cx)
                })
                .map(|(_, persisted)| persisted)
            } else {
                None
            };
            fired.push((message.content, persisted));
        }
        self.arm_scheduled_message_timer(window, cx);
        if fired.is_empty() {
            return;
        }
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            for (content, persisted) in fired {
                if let Some(persisted) = persisted {
                    persisted.await;
                }
                this.update_in(cx, |this, window, cx| {
                    this.add_to_queue(content, Vec::new(), window, cx);
                })
                .ok();
            }
            this.update_in(cx, |this, window, cx| {
                if this.thread.read(cx).status() == ThreadStatus::Idle
                    && let Some(front_id) = this.message_queue.first_id()
                {
                    this.send_queued_message_now(front_id, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Fires one entry immediately, regardless of its fire time, through
    /// the shared fire routine — so the delete-before-enqueue ordering and
    /// the idle-dispatch rule hold here exactly as for a timer-driven fire.
    pub(crate) fn send_scheduled_message_now(
        &mut self,
        id: &ScheduledMessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fire_scheduled_messages(vec![id.clone()], window, cx);
    }

    /// Cancels one entry and returns its content to the main message
    /// editor: the withdrawn message has no schedule anymore, and sending
    /// it again requires picking a new time (spec §4). Mirrors
    /// `move_queued_message_to_main_editor`: an empty editor takes the
    /// content wholesale, a non-empty editor gets it appended after a
    /// blank line, and the editor is focused either way.
    ///
    /// The store write is fire-and-forget: unlike the fire path nothing is
    /// sent afterwards, so a write lost to a crash can at worst resurrect
    /// the entry on restart — never double-send.
    pub(crate) fn withdraw_scheduled_message(
        &mut self,
        id: &ScheduledMessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(message) = self.remove_scheduled_message_everywhere(id, window, cx) else {
            return;
        };
        let message_editor = self.message_editor.clone();
        window.focus(&message_editor.focus_handle(cx), cx);
        if message_editor.read(cx).is_empty(cx) {
            message_editor.update(cx, |editor, cx| {
                editor.set_message(message.content, window, cx);
            });
        } else {
            message_editor.update(cx, |editor, cx| {
                editor.append_message(message.content, Some("\n\n"), window, cx);
            });
        }
    }

    /// Discards one entry: record removed, timer re-armed, nothing sent
    /// and no editor interaction (spec §4).
    pub(crate) fn delete_scheduled_message(
        &mut self,
        id: &ScheduledMessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remove_scheduled_message_everywhere(id, window, cx);
    }

    /// Removes one entry from the runtime set and the persisted store and
    /// re-arms the timer — the shared tail of withdraw and delete. The
    /// store write is fire-and-forget (see `withdraw_scheduled_message`).
    fn remove_scheduled_message_everywhere(
        &mut self,
        id: &ScheduledMessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<ScheduledMessage> {
        let message = self.scheduled.remove_entry(id)?;
        if cx.try_global::<ScheduledMessageStore>().is_some() {
            ScheduledMessageStore::update_global(cx, |store, cx| {
                store.remove(&self.scheduled.session_key, id, cx);
            });
        }
        self.arm_scheduled_message_timer(window, cx);
        cx.notify();
        Some(message)
    }

    /// Schedules the main editor's current content to fire at `fire_at`:
    /// resolves the content blocks, persists a new entry, adds it to the
    /// runtime set with a display editor (like a restored entry), clears
    /// the editor, and re-arms the timer. An empty editor is a no-op.
    ///
    /// Tracked buffers are deliberately dropped — they only feed
    /// stale-buffer detection and are never persisted (spec §2.1); the
    /// content blocks carry the mentions.
    pub(crate) fn schedule_current_message(
        &mut self,
        fire_at: DateTime<Utc>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let message_editor = self.message_editor.clone();
        if message_editor.read(cx).is_empty(cx) {
            return;
        }
        // Resolve before clearing: the resolve task reads the editor
        // lazily, so clearing first would wipe the contents.
        let contents = self.resolve_message_contents(&message_editor, cx);
        cx.spawn_in(window, async move |this, cx| {
            let (content, _tracked_buffers) = contents.await?;
            if content.is_empty() {
                return Ok(());
            }
            this.update_in(cx, |this, window, cx| {
                let message = ScheduledMessage {
                    id: ScheduledMessageId::new(),
                    content,
                    fire_at,
                    created_at: Utc::now(),
                };
                if cx.try_global::<ScheduledMessageStore>().is_some() {
                    ScheduledMessageStore::update_global(cx, |store, cx| {
                        store.add(this.scheduled.session_key.clone(), message.clone(), cx);
                    });
                }
                let editor =
                    this.build_scheduled_display_editor(message.content.clone(), window, cx);
                this.scheduled.editors.insert(message.id.clone(), editor);
                this.scheduled.set.add(message);
                message_editor.update(cx, |editor, cx| editor.clear(window, cx));
                this.arm_scheduled_message_timer(window, cx);
                cx.notify();
            })?;
            Ok::<(), anyhow::Error>(())
        })
        .detach_and_log_err(cx);
    }

    /// The right half of the send split button (spec §6.1): a chevron that
    /// opens the preset dropdown, plus the trigger-less popover hosting the
    /// custom-time input, shown programmatically from the dropdown.
    pub(crate) fn render_send_dropdown(
        &self,
        is_editor_empty: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let menu_open = self.scheduled.schedule_menu_handle.is_deployed();
        let chevron = ButtonLike::new_rounded_right("schedule-message-trigger")
            .layer(ElevationIndex::ModalSurface)
            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
            .width(rems_from_px(20.))
            .height(rems_from_px(20.).into())
            .child(
                Icon::new(if menu_open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall),
            )
            .tooltip(Tooltip::text("Send Later"));

        let weak_menu = cx.weak_entity();
        let weak_picker = cx.weak_entity();
        h_flex()
            .child(
                PopoverMenu::new("schedule-message-menu")
                    .trigger(chevron)
                    .anchor(gpui::Anchor::BottomRight)
                    .with_handle(self.scheduled.schedule_menu_handle.clone())
                    .offset(gpui::Point {
                        x: px(0.0),
                        y: px(-2.0),
                    })
                    .menu(move |window, cx| {
                        weak_menu
                            .update(cx, |this, cx| {
                                this.build_schedule_menu(is_editor_empty, window, cx)
                            })
                            .ok()
                    }),
            )
            .child(
                PopoverMenu::new("schedule-time-picker")
                    .anchor(gpui::Anchor::BottomRight)
                    .with_handle(self.scheduled.time_picker_handle.clone())
                    .offset(gpui::Point {
                        x: px(0.0),
                        y: px(-2.0),
                    })
                    .menu(move |window, cx| {
                        Some(cx.new(|cx| ScheduleTimePicker::new(weak_picker.clone(), window, cx)))
                    }),
            )
            .into_any_element()
    }

    /// The preset dropdown (spec §5): each item computes its instant at
    /// click time from the wall clock, so every preset is strictly in the
    /// future; "Tonight at 22:00" is hidden once 22:00 local has passed.
    fn build_schedule_menu(
        &self,
        is_editor_empty: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        let weak = cx.weak_entity();
        let picker_handle = self.scheduled.time_picker_handle.clone();
        let show_tonight = Local::now().time() < chrono::NaiveTime::from_hms_opt(22, 0, 0).unwrap();

        let preset = |label: &'static str,
                      fire_at: fn() -> Option<DateTime<Utc>>|
         -> ContextMenuEntry {
            let weak = weak.clone();
            ContextMenuEntry::new(label)
                .disabled(is_editor_empty)
                .handler(move |window, cx| {
                    weak.update(cx, |this, cx| {
                        // `fire_at` reads the clock now, at click time,
                        // not at menu-build time; `None` only for local
                        // times skipped by a DST transition. The filter
                        // keeps the instant strictly in the future, which
                        // `schedule_current_message` requires: a menu left
                        // open across the preset's wall-clock boundary
                        // (e.g. past 22:00 with "Tonight at 22:00" still
                        // showing) must click through as a no-op rather
                        // than schedule a time in the past.
                        if let Some(fire_at) = fire_at().filter(|fire_at| *fire_at > Utc::now()) {
                            this.schedule_current_message(fire_at, window, cx);
                        }
                    })
                    .ok();
                })
        };

        ContextMenu::build(window, cx, move |menu, _window, _cx| {
            menu.key_context("ScheduleMessageMenu")
                .header("Send Later")
                .item(preset("In 30 minutes", || {
                    Some((Local::now() + chrono::Duration::minutes(30)).with_timezone(&Utc))
                }))
                .item(preset("In 1 hour", || {
                    Some((Local::now() + chrono::Duration::hours(1)).with_timezone(&Utc))
                }))
                .when(show_tonight, |menu| {
                    menu.item(preset("Tonight at 22:00", || {
                        local_date_time_to_utc(
                            Local::now().date_naive(),
                            chrono::NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
                        )
                    }))
                })
                .item(preset("Tomorrow at 9:00", || {
                    local_date_time_to_utc(
                        Local::now().date_naive().succ_opt()?,
                        chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
                    )
                }))
                .separator()
                .item(
                    ContextMenuEntry::new("Custom time…")
                        .disabled(is_editor_empty)
                        .handler(move |window, cx| {
                            let picker_handle = picker_handle.clone();
                            // Deferred so the picker opens after this menu's
                            // dismissal has finished, winning the focus race.
                            window.defer(cx, move |window, cx| {
                                picker_handle.show(window, cx);
                            });
                        }),
                )
        })
    }

    /// The scheduled block (spec §6.2), mounted directly below the message
    /// queue in the activity bar; the caller skips it entirely when the set
    /// is empty. `has_section_above` adds the divider toward the preceding
    /// activity-bar section.
    pub(crate) fn render_scheduled_messages_block(
        &self,
        has_section_above: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let count = self.scheduled.set.len();
        let title: SharedString = if count == 1 {
            "1 Scheduled Message".into()
        } else {
            format!("{count} Scheduled Messages").into()
        };
        let now = Utc::now();
        let now_local = Local::now();

        v_flex()
            .when(has_section_above, |this| {
                this.border_t_1().border_color(cx.theme().colors().border)
            })
            .child(
                h_flex()
                    .p_1()
                    .w_full()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        // Matches the horizontal footprint of the queue
                        // summary's `Disclosure` (an `IconButton`, whose
                        // default size pads the icon by `Base04` per side),
                        // so this title left-aligns with the queue title.
                        h_flex().px(DynamicSpacing::Base04.rems(cx)).child(
                            Icon::new(IconName::Clock)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                    .child(Label::new(title).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(
                v_flex()
                    .id("scheduled_message_list")
                    .max_h_40()
                    .overflow_y_scroll()
                    .children(
                        self.scheduled
                            .set
                            .iter()
                            .enumerate()
                            .map(|(index, message)| {
                                let id = message.id.clone();
                                let editor = self.scheduled.editors.get(&message.id).cloned();
                                let effective = self.scheduled.effective_fire_at(message);
                                // Per-entry overdue judgement: the grace window can
                                // hold some entries while future ones coexist, so
                                // `overdue_grace.is_some()` alone would mislabel.
                                let is_overdue = effective != message.fire_at;
                                let meta_label = if is_overdue {
                                    let seconds = (effective - now).num_seconds().max(0);
                                    Label::new(format!("Overdue — sending in {seconds}s…"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning)
                                } else {
                                    Label::new(format!(
                                        "{} · {}",
                                        fire_time_label(message.fire_at, now_local),
                                        format_countdown((effective - now).num_seconds()),
                                    ))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                };

                                h_flex()
                                    .group("scheduled_entry")
                                    .w_full()
                                    .p_1p5()
                                    .gap_1()
                                    .bg(cx.theme().colors().editor_background)
                                    .when(index < count - 1, |this| {
                                        this.border_b_1()
                                            .border_color(cx.theme().colors().border_variant)
                                    })
                                    .child(Icon::new(IconName::Clock).size(IconSize::Small).color(
                                        if is_overdue {
                                            Color::Warning
                                        } else {
                                            Color::Muted
                                        },
                                    ))
                                    .child(
                                        div().flex_1().min_w_0().relative().children(editor).child(
                                            // Transparent overlay so a body
                                            // click withdraws instead of
                                            // focusing the read-only editor.
                                            // Text cursor, matching what the
                                            // queue rows' editors show.
                                            div()
                                                .id(("scheduled-withdraw", index))
                                                .absolute()
                                                .inset_0()
                                                .cursor_text()
                                                .tooltip(Tooltip::text("Move Back to Editor"))
                                                .on_click(cx.listener({
                                                    let id = id.clone();
                                                    move |this, _, window, cx| {
                                                        this.withdraw_scheduled_message(
                                                            &id, window, cx,
                                                        );
                                                    }
                                                })),
                                        ),
                                    )
                                    .child(
                                        h_flex().gap_1().justify_end().child(meta_label).child(
                                            // Same order as the queue rows'
                                            // hover actions: destructive
                                            // first, send-now rightmost.
                                            h_flex()
                                                .visible_on_hover("scheduled_entry")
                                                .gap_1()
                                                .child(
                                                    IconButton::new(
                                                        ("scheduled-delete", index),
                                                        IconName::Trash,
                                                    )
                                                    .icon_size(IconSize::Small)
                                                    .tooltip(Tooltip::text(
                                                        "Delete Scheduled Message",
                                                    ))
                                                    .on_click(cx.listener({
                                                        let id = id.clone();
                                                        move |this, _, window, cx| {
                                                            this.delete_scheduled_message(
                                                                &id, window, cx,
                                                            );
                                                        }
                                                    })),
                                                )
                                                .child(
                                                    IconButton::new(
                                                        ("scheduled-send-now", index),
                                                        IconName::Send,
                                                    )
                                                    .icon_size(IconSize::Small)
                                                    .tooltip(Tooltip::text("Send Now"))
                                                    .on_click(cx.listener({
                                                        let id = id.clone();
                                                        move |this, _, window, cx| {
                                                            this.send_scheduled_message_now(
                                                                &id, window, cx,
                                                            );
                                                        }
                                                    })),
                                                ),
                                        ),
                                    )
                            }),
                    ),
            )
    }
}

/// The fire-time label of a scheduled entry: "Today 22:00", "Tomorrow
/// 09:00", or the full date for anything further out.
fn fire_time_label(fire_at: DateTime<Utc>, now_local: DateTime<Local>) -> String {
    let local = fire_at.with_timezone(&Local);
    let date = local.date_naive();
    let today = now_local.date_naive();
    if date == today {
        format!("Today {}", local.format("%H:%M"))
    } else if Some(date) == today.succ_opt() {
        format!("Tomorrow {}", local.format("%H:%M"))
    } else {
        local.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// A compact live countdown, two units at most: "in 2d 3h", "in 1h 05m",
/// "in 4m 32s", "in 42s".
fn format_countdown(total_seconds: i64) -> String {
    let total = total_seconds.max(0);
    let (days, hours, minutes, seconds) = (
        total / 86_400,
        total / 3_600 % 24,
        total / 60 % 60,
        total % 60,
    );
    if days > 0 {
        format!("in {days}d {hours}h")
    } else if hours > 0 {
        format!("in {hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("in {minutes}m {seconds:02}s")
    } else {
        format!("in {seconds}s")
    }
}

/// The inline preview under the custom-time input: what the entered time
/// resolves to, phrased relative to today.
fn schedule_preview(fire_at: DateTime<Utc>, now_local: DateTime<Local>) -> String {
    let local = fire_at.with_timezone(&Local);
    let date = local.date_naive();
    let today = now_local.date_naive();
    if date == today {
        format!("Will send today at {}", local.format("%H:%M"))
    } else if Some(date) == today.succ_opt() {
        format!("Will send tomorrow at {}", local.format("%H:%M"))
    } else {
        format!("Will send on {}", local.format("%Y-%m-%d at %H:%M"))
    }
}

/// The custom-time popover (spec §6.1): a single-line input parsed on every
/// keystroke via [`parse_fire_time`], with the resolved preview or the parse
/// error shown inline underneath. Enter schedules, Escape dismisses. Hosted
/// by a trigger-less [`PopoverMenu`] like the crate's other pickers, and
/// reuses the feedback-editor pattern of a container handling `menu::Confirm`
/// / `menu::Cancel` around a single-line editor.
pub struct ScheduleTimePicker {
    editor: Entity<Editor>,
    thread_view: WeakEntity<ThreadView>,
    parsed: Result<DateTime<Utc>, ParseError>,
    _editor_subscription: Subscription,
}

impl ScheduleTimePicker {
    fn new(
        thread_view: WeakEntity<ThreadView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("30m, 21:30, tomorrow 09:00…", window, cx);
            editor
        });
        let subscription = cx.subscribe(&editor, |this: &mut Self, _, event, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                this.parsed = this.parse(cx);
                cx.notify();
            }
        });
        Self {
            editor,
            thread_view,
            parsed: Err(ParseError::Empty),
            _editor_subscription: subscription,
        }
    }

    fn parse(&self, cx: &App) -> Result<DateTime<Utc>, ParseError> {
        parse_fire_time(&self.editor.read(cx).text(cx), Local::now())
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        // Re-parse against the current clock: the displayed preview may be
        // stale enough that its instant no longer clears the minimum lead.
        self.parsed = self.parse(cx);
        match self.parsed {
            Ok(fire_at) => {
                self.thread_view
                    .update(cx, |thread_view, cx| {
                        thread_view.schedule_current_message(fire_at, window, cx);
                    })
                    .ok();
                cx.emit(DismissEvent);
            }
            Err(_) => cx.notify(),
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for ScheduleTimePicker {}

impl Focusable for ScheduleTimePicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for ScheduleTimePicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (feedback, feedback_color) = match &self.parsed {
            Ok(fire_at) => (schedule_preview(*fire_at, Local::now()), Color::Muted),
            Err(error @ ParseError::Empty) => (error.to_string(), Color::Muted),
            Err(error) => (error.to_string(), Color::Error),
        };

        v_flex()
            .key_context("ScheduleTimePicker")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .elevation_2(cx)
            .w(rems(22.))
            .p_1()
            .gap_1()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(self.editor.clone()),
            )
            .child(
                h_flex().px_1().child(
                    Label::new(feedback)
                        .size(LabelSize::XSmall)
                        .color(feedback_color),
                ),
            )
    }
}

/// Why an input string could not be turned into a fire time. The `Display`
/// text is shown inline in the custom-time input as the user types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    Unrecognized,
    TooSoon,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "Enter a time, e.g. 30m, 21:30, or tomorrow 09:00"),
            Self::Unrecognized => {
                write!(
                    f,
                    "Unrecognized time; use 30m, 1h30m, 21:30, or tomorrow 09:00"
                )
            }
            Self::TooSoon => write!(f, "Time must be at least 10 seconds from now"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Minimum lead required between "now" and the requested fire time.
/// See docs/scheduled-messages.spec.md §5.
const MIN_LEAD_SECONDS: i64 = 10;

/// Parses user input into a UTC fire time. Grammar (spec §5):
///
/// - relative offsets: `30m`, `2h`, `1h30m`, `1d` — units d/h/m, combinable
///   in day → hour → minute order
/// - absolute wall-clock: `HH:MM` (rolls to tomorrow if already past today),
///   `tomorrow HH:MM`
///
/// `now_local` is injected rather than read from the clock so callers (and
/// tests) control the reference instant; the result is rejected if it is less
/// than ten seconds after it.
pub fn parse_fire_time(
    input: &str,
    now_local: DateTime<Local>,
) -> Result<DateTime<Utc>, ParseError> {
    let input = input.trim().to_ascii_lowercase();
    if input.is_empty() {
        return Err(ParseError::Empty);
    }

    let fire_at = if let Some(offset) = parse_relative_offset(&input) {
        now_local
            .checked_add_signed(offset)
            .ok_or(ParseError::Unrecognized)?
            .with_timezone(&Utc)
    } else if let Some(rest) = input.strip_prefix("tomorrow ") {
        let time = parse_wall_clock(rest.trim_start()).ok_or(ParseError::Unrecognized)?;
        let tomorrow = now_local
            .date_naive()
            .succ_opt()
            .ok_or(ParseError::Unrecognized)?;
        local_date_time_to_utc(tomorrow, time).ok_or(ParseError::Unrecognized)?
    } else if let Some(time) = parse_wall_clock(&input) {
        let today = now_local.date_naive();
        match local_date_time_to_utc(today, time) {
            // "Already past" keeps its plain meaning here: a time a few
            // seconds ahead stays today and falls to the minimum-lead check
            // below, rather than silently meaning tomorrow.
            Some(candidate) if candidate > now_local.with_timezone(&Utc) => candidate,
            _ => {
                let tomorrow = today.succ_opt().ok_or(ParseError::Unrecognized)?;
                local_date_time_to_utc(tomorrow, time).ok_or(ParseError::Unrecognized)?
            }
        }
    } else {
        return Err(ParseError::Unrecognized);
    };

    if fire_at < now_local.with_timezone(&Utc) + chrono::Duration::seconds(MIN_LEAD_SECONDS) {
        return Err(ParseError::TooSoon);
    }
    Ok(fire_at)
}

/// Parses `[Nd][Nh][Nm]` with at least one component present, e.g. `30m`,
/// `2h`, `1h30m`, `1d2h3m`. Components must appear in day → hour → minute
/// order and the whole input must be consumed.
fn parse_relative_offset(input: &str) -> Option<chrono::Duration> {
    let mut rest = input.as_bytes();
    let mut minutes: u64 = 0;
    let mut matched_any = false;
    for (unit, minutes_per_unit) in [(b'd', 24 * 60), (b'h', 60), (b'm', 1)] {
        let digit_count = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if digit_count == 0 || rest.get(digit_count) != Some(&unit) {
            continue;
        }
        let value: u64 = std::str::from_utf8(&rest[..digit_count])
            .ok()?
            .parse()
            .ok()?;
        minutes = minutes.checked_add(value.checked_mul(minutes_per_unit)?)?;
        rest = &rest[digit_count + 1..];
        matched_any = true;
    }
    if !matched_any || !rest.is_empty() {
        return None;
    }
    chrono::Duration::try_minutes(i64::try_from(minutes).ok()?)
}

/// Parses `HH:MM` (hour may be one or two digits, minutes exactly two).
fn parse_wall_clock(input: &str) -> Option<chrono::NaiveTime> {
    let (hour_text, minute_text) = input.split_once(':')?;
    if hour_text.is_empty()
        || hour_text.len() > 2
        || minute_text.len() != 2
        || !hour_text.bytes().all(|byte| byte.is_ascii_digit())
        || !minute_text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    chrono::NaiveTime::from_hms_opt(hour_text.parse().ok()?, minute_text.parse().ok()?, 0)
}

/// Resolves a local calendar date + wall-clock time to a UTC instant. Returns
/// `None` for times skipped by a DST transition; picks the earlier instant of
/// ambiguous (repeated) times.
fn local_date_time_to_utc(
    date: chrono::NaiveDate,
    time: chrono::NaiveTime,
) -> Option<DateTime<Utc>> {
    Local
        .from_local_datetime(&date.and_time(time))
        .earliest()
        .map(|local| local.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1 as acp;
    use chrono::{DateTime, Local, Utc};
    use collections::HashMap;
    use db::kvp::KeyValueStore;
    use gpui::{ReadGlobal as _, TestAppContext};

    /// Fixed "now" for parser tests: 2026-08-25 12:00:00 local time. Tests
    /// convert expected values through `Local` the same way the parser does,
    /// so they hold in any machine timezone.
    fn fixed_now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 8, 25, 12, 0, 0).unwrap()
    }

    fn local_utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Local
            .with_ymd_and_hms(y, mo, d, h, mi, s)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn test_scheduled_messages_file_roundtrip() {
        let message = ScheduledMessage {
            id: ScheduledMessageId::new(),
            content: vec![
                acp::ContentBlock::Text(acp::TextContent::new("check the deploy status")),
                acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
                    "main.rs",
                    "file:///project/src/main.rs",
                )),
            ],
            fire_at: Utc.with_ymd_and_hms(2026, 8, 25, 14, 0, 0).unwrap(),
            created_at: Utc.with_ymd_and_hms(2026, 8, 25, 12, 0, 0).unwrap(),
        };
        let mut sessions = HashMap::default();
        sessions.insert("zed|session-123".to_string(), vec![message]);
        let file = ScheduledMessagesFile {
            version: 1,
            sessions,
        };

        let json = serde_json::to_string(&file).unwrap();
        let deserialized: ScheduledMessagesFile = serde_json::from_str(&json).unwrap();
        assert_eq!(file, deserialized);
    }

    #[test]
    fn test_parse_relative_minutes() {
        assert_eq!(
            parse_fire_time("30m", fixed_now()),
            Ok(local_utc(2026, 8, 25, 12, 30, 0))
        );
    }

    #[test]
    fn test_parse_relative_hours() {
        assert_eq!(
            parse_fire_time("2h", fixed_now()),
            Ok(local_utc(2026, 8, 25, 14, 0, 0))
        );
    }

    #[test]
    fn test_parse_relative_days() {
        assert_eq!(
            parse_fire_time("1d", fixed_now()),
            Ok(local_utc(2026, 8, 26, 12, 0, 0))
        );
    }

    #[test]
    fn test_parse_relative_combined_units() {
        assert_eq!(
            parse_fire_time("1h30m", fixed_now()),
            Ok(local_utc(2026, 8, 25, 13, 30, 0))
        );
        assert_eq!(
            parse_fire_time("1d2h3m", fixed_now()),
            Ok(local_utc(2026, 8, 26, 14, 3, 0))
        );
        // Units out of day -> hour -> minute order are not part of the grammar.
        assert!(parse_fire_time("30m1h", fixed_now()).is_err());
    }

    #[test]
    fn test_parse_trims_whitespace() {
        assert_eq!(
            parse_fire_time("  30m  ", fixed_now()),
            Ok(local_utc(2026, 8, 25, 12, 30, 0))
        );
    }

    #[test]
    fn test_parse_absolute_time_later_today() {
        assert_eq!(
            parse_fire_time("22:00", fixed_now()),
            Ok(local_utc(2026, 8, 25, 22, 0, 0))
        );
    }

    #[test]
    fn test_parse_absolute_time_already_past_rolls_to_tomorrow() {
        // 09:00 is before the fixed noon "now", so it means tomorrow 09:00.
        assert_eq!(
            parse_fire_time("09:00", fixed_now()),
            Ok(local_utc(2026, 8, 26, 9, 0, 0))
        );
    }

    #[test]
    fn test_parse_tomorrow_with_time() {
        assert_eq!(
            parse_fire_time("tomorrow 09:00", fixed_now()),
            Ok(local_utc(2026, 8, 26, 9, 0, 0))
        );
        // "tomorrow" matches case-insensitively.
        assert_eq!(
            parse_fire_time("Tomorrow 21:30", fixed_now()),
            Ok(local_utc(2026, 8, 26, 21, 30, 0))
        );
    }

    #[test]
    fn test_parse_rejects_garbage() {
        assert_eq!(
            parse_fire_time("banana", fixed_now()),
            Err(ParseError::Unrecognized)
        );
        assert_eq!(
            parse_fire_time("25:99", fixed_now()),
            Err(ParseError::Unrecognized)
        );
        assert_eq!(
            parse_fire_time("tomorrow", fixed_now()),
            Err(ParseError::Unrecognized)
        );
    }

    #[test]
    fn test_parse_rejects_empty_input() {
        assert_eq!(parse_fire_time("", fixed_now()), Err(ParseError::Empty));
        assert_eq!(parse_fire_time("   ", fixed_now()), Err(ParseError::Empty));
    }

    #[test]
    fn test_parse_rejects_zero_relative_offset() {
        // "0m" parses but lands inside the minimum ten-second lead window.
        assert_eq!(parse_fire_time("0m", fixed_now()), Err(ParseError::TooSoon));
    }

    #[test]
    fn test_parse_rejects_absolute_time_within_lead_window() {
        // An absolute time a few seconds ahead is not "already past", so it
        // does not roll to tomorrow; it is rejected for being under the
        // ten-second minimum lead instead.
        let now = Local.with_ymd_and_hms(2026, 8, 25, 11, 59, 55).unwrap();
        assert_eq!(parse_fire_time("12:00", now), Err(ParseError::TooSoon));
    }

    fn message_at(fire_at: DateTime<Utc>, created_at: DateTime<Utc>) -> ScheduledMessage {
        ScheduledMessage {
            id: ScheduledMessageId::new(),
            content: vec![acp::ContentBlock::Text(acp::TextContent::new("hello"))],
            fire_at,
            created_at,
        }
    }

    fn utc(h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 25, h, mi, 0).unwrap()
    }

    #[test]
    fn test_set_orders_entries_by_fire_time_after_unordered_adds() {
        let mut set = ScheduledMessageSet::default();
        assert!(set.is_empty());
        set.add(message_at(utc(15, 0), utc(10, 0)));
        set.add(message_at(utc(13, 0), utc(10, 1)));
        set.add(message_at(utc(14, 0), utc(10, 2)));
        assert_eq!(set.len(), 3);
        let fire_times: Vec<_> = set.iter().map(|message| message.fire_at).collect();
        assert_eq!(fire_times, vec![utc(13, 0), utc(14, 0), utc(15, 0)]);
    }

    #[test]
    fn test_set_breaks_fire_time_ties_by_created_at() {
        let mut set = ScheduledMessageSet::default();
        let later_created = message_at(utc(13, 0), utc(11, 0));
        let earlier_created = message_at(utc(13, 0), utc(10, 0));
        set.add(later_created.clone());
        set.add(earlier_created.clone());
        let ids: Vec<_> = set.iter().map(|message| message.id.clone()).collect();
        assert_eq!(ids, vec![earlier_created.id, later_created.id]);
    }

    #[test]
    fn test_set_remove_and_get() {
        let mut set = ScheduledMessageSet::default();
        let message = message_at(utc(13, 0), utc(10, 0));
        let id = message.id.clone();
        set.add(message.clone());
        assert_eq!(set.get(&id), Some(&message));

        let removed = set.remove(&id);
        assert_eq!(removed, Some(message));
        assert_eq!(set.get(&id), None);
        assert!(set.is_empty());
        assert_eq!(set.remove(&id), None);
    }

    #[test]
    fn test_set_due_entries_boundary_and_order() {
        let mut set = ScheduledMessageSet::default();
        let due_exactly_now = message_at(utc(12, 0), utc(10, 0));
        let overdue = message_at(utc(11, 0), utc(10, 0));
        let future = message_at(utc(13, 0), utc(10, 0));
        set.add(due_exactly_now.clone());
        set.add(overdue.clone());
        set.add(future);

        // fire_at == now counts as due; results come back in fire order.
        let due = set.due_entries(utc(12, 0));
        assert_eq!(due, vec![overdue.id, due_exactly_now.id]);
        assert!(set.due_entries(utc(10, 0)).is_empty());
    }

    #[test]
    fn test_set_next_fire_at() {
        let mut set = ScheduledMessageSet::default();
        assert_eq!(set.next_fire_at(), None);
        set.add(message_at(utc(15, 0), utc(10, 0)));
        set.add(message_at(utc(13, 0), utc(10, 0)));
        assert_eq!(set.next_fire_at(), Some(utc(13, 0)));
    }

    fn file_with_session(key: &str, messages: Vec<ScheduledMessage>) -> ScheduledMessagesFile {
        let mut sessions = HashMap::default();
        sessions.insert(key.to_string(), messages);
        ScheduledMessagesFile {
            version: SCHEDULED_MESSAGES_FILE_VERSION,
            sessions,
        }
    }

    #[test]
    fn test_gc_drops_messages_whose_fire_time_expired_over_thirty_days_ago() {
        let now = utc(12, 0);
        let expired = message_at(
            now - chrono::Duration::days(31),
            now - chrono::Duration::days(40),
        );
        let file = file_with_session("zed|stale-session", vec![expired]);

        let collected = gc(file, now);
        assert!(collected.sessions.is_empty());
        assert_eq!(collected.version, SCHEDULED_MESSAGES_FILE_VERSION);
    }

    #[test]
    fn test_gc_keeps_overdue_messages_within_thirty_days() {
        let now = utc(12, 0);
        // Overdue but not expired: fires as catch-up when its thread reopens.
        let overdue = message_at(
            now - chrono::Duration::days(29),
            now - chrono::Duration::days(40),
        );
        let file = file_with_session("zed|session", vec![overdue.clone()]);

        let collected = gc(file, now);
        assert_eq!(collected.sessions["zed|session"], vec![overdue]);
    }

    #[test]
    fn test_gc_keeps_future_messages_regardless_of_creation_age() {
        let now = utc(12, 0);
        // Scheduled far in advance long ago: TTL keys off fire_at, so age of
        // created_at alone never expires an entry.
        let future = message_at(
            now + chrono::Duration::days(2),
            now - chrono::Duration::days(300),
        );
        let file = file_with_session("zed|session", vec![future.clone()]);

        let collected = gc(file, now);
        assert_eq!(collected.sessions["zed|session"], vec![future]);
    }

    /// Gives `KeyValueStore::global` an isolated in-memory database so store
    /// tests don't leak state into each other or the developer's real
    /// database. Same setup as the kvp tests in `agent_ui.rs`.
    fn init_test_db(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
    }

    /// Writes a raw string to the store's single KV key, bypassing the store,
    /// to simulate pre-existing (or corrupted) persisted state.
    async fn seed_raw_file(value: &str, cx: &mut TestAppContext) {
        let kvp = cx.update(|cx| KeyValueStore::global(cx));
        kvp.scoped(NAMESPACE)
            .write(FILE_KEY.to_string(), value.to_string())
            .await
            .unwrap();
    }

    #[gpui::test]
    async fn test_store_round_trip_isolates_sessions(cx: &mut TestAppContext) {
        init_test_db(cx);
        cx.update(ScheduledMessageStore::init);

        let now = Utc::now();
        let message_a = message_at(now + chrono::Duration::hours(2), now);
        let message_b = message_at(now + chrono::Duration::hours(3), now);
        cx.update(|cx| {
            ScheduledMessageStore::update_global(cx, |store, cx| {
                store.add("zed|session-a".to_string(), message_a.clone(), cx);
                store.add("claude|session-b".to_string(), message_b.clone(), cx);
            })
        });
        cx.run_until_parked();

        // A fresh store built from the same database sees both sessions,
        // each holding only its own message.
        cx.update(ScheduledMessageStore::init);
        cx.update(|cx| {
            let store = ScheduledMessageStore::global(cx);
            assert_eq!(store.messages_for("zed|session-a"), vec![message_a]);
            assert_eq!(store.messages_for("claude|session-b"), vec![message_b]);
        });
    }

    #[gpui::test]
    async fn test_store_load_drops_expired_entries(cx: &mut TestAppContext) {
        init_test_db(cx);
        let now = Utc::now();
        let expired = message_at(
            now - chrono::Duration::days(31),
            now - chrono::Duration::days(40),
        );
        let valid = message_at(now + chrono::Duration::days(1), now);
        let mut sessions = HashMap::default();
        sessions.insert("zed|stale".to_string(), vec![expired]);
        sessions.insert("zed|live".to_string(), vec![valid.clone()]);
        let file = ScheduledMessagesFile {
            version: SCHEDULED_MESSAGES_FILE_VERSION,
            sessions,
        };
        seed_raw_file(&serde_json::to_string(&file).unwrap(), cx).await;

        cx.update(ScheduledMessageStore::init);
        cx.update(|cx| {
            let store = ScheduledMessageStore::global(cx);
            assert_eq!(store.messages_for("zed|live"), vec![valid]);
            assert!(store.messages_for("zed|stale").is_empty());
        });
    }

    #[gpui::test]
    async fn test_store_loads_empty_on_corrupted_value(cx: &mut TestAppContext) {
        init_test_db(cx);
        seed_raw_file("not json", cx).await;

        // Must not panic; the unreadable value is dropped.
        cx.update(ScheduledMessageStore::init);
        cx.update(|cx| {
            assert!(ScheduledMessageStore::global(cx).file.sessions.is_empty());
        });
    }

    #[gpui::test]
    async fn test_store_load_leaves_corrupted_value_untouched_on_disk(cx: &mut TestAppContext) {
        init_test_db(cx);
        seed_raw_file("not json", cx).await;

        cx.update(ScheduledMessageStore::init);
        cx.run_until_parked();

        // Loading starts the store empty in memory but must not write that
        // emptiness back: the unreadable value stays on disk (where a future
        // build that understands it again could still recover it) until some
        // later mutation legitimately overwrites the key.
        let kvp = cx.update(|cx| KeyValueStore::global(cx));
        let raw = kvp.scoped(NAMESPACE).read(FILE_KEY).unwrap();
        assert_eq!(raw.as_deref(), Some("not json"));
    }

    #[gpui::test]
    async fn test_store_loads_empty_on_unsupported_version(cx: &mut TestAppContext) {
        init_test_db(cx);
        let now = Utc::now();
        let file = ScheduledMessagesFile {
            version: SCHEDULED_MESSAGES_FILE_VERSION + 1,
            ..file_with_session(
                "zed|session",
                vec![message_at(now + chrono::Duration::days(1), now)],
            )
        };
        seed_raw_file(&serde_json::to_string(&file).unwrap(), cx).await;

        cx.update(ScheduledMessageStore::init);
        cx.update(|cx| {
            assert!(ScheduledMessageStore::global(cx).file.sessions.is_empty());
        });
    }

    #[gpui::test]
    async fn test_store_remove_drops_empty_session(cx: &mut TestAppContext) {
        init_test_db(cx);
        cx.update(ScheduledMessageStore::init);
        let now = Utc::now();
        let message = message_at(now + chrono::Duration::hours(1), now);
        let id = message.id.clone();
        cx.update(|cx| {
            ScheduledMessageStore::update_global(cx, |store, cx| {
                store.add("zed|session".to_string(), message.clone(), cx);
            })
        });

        let removed = cx.update(|cx| {
            ScheduledMessageStore::update_global(cx, |store, cx| {
                store.remove("zed|session", &id, cx)
            })
        });
        assert_eq!(
            removed.map(|(removed_message, _)| removed_message),
            Some(message)
        );
        cx.update(|cx| {
            let store = ScheduledMessageStore::global(cx);
            assert!(!store.file.sessions.contains_key("zed|session"));
        });

        // Removing the same id again finds nothing.
        let removed = cx.update(|cx| {
            ScheduledMessageStore::update_global(cx, |store, cx| {
                store.remove("zed|session", &id, cx)
            })
        });
        assert_eq!(removed.map(|(removed_message, _)| removed_message), None);

        // The emptied session is gone from a fresh reload of the database
        // too, not just from memory.
        cx.run_until_parked();
        cx.update(ScheduledMessageStore::init);
        cx.update(|cx| {
            assert!(ScheduledMessageStore::global(cx).file.sessions.is_empty());
        });
    }

    #[gpui::test]
    async fn test_store_remove_persistence_task_resolves_after_write_lands(
        cx: &mut TestAppContext,
    ) {
        init_test_db(cx);
        cx.update(ScheduledMessageStore::init);
        let message = message_at(utc(14, 0), utc(12, 0));
        let id = message.id.clone();
        cx.update(|cx| {
            ScheduledMessageStore::update_global(cx, |store, cx| {
                store.add("zed|session".to_string(), message, cx);
            })
        });

        let (_, persisted) = cx
            .update(|cx| {
                ScheduledMessageStore::update_global(cx, |store, cx| {
                    store.remove("zed|session", &id, cx)
                })
            })
            .expect("the entry exists");
        persisted.await;

        // Once the task resolves — with no further executor pumping — a
        // store reloaded from the database must already see the removal.
        // The fire path relies on this ordering: enqueueing before the
        // delete is durable could double-send after a crash.
        cx.update(ScheduledMessageStore::init);
        cx.update(|cx| {
            assert!(ScheduledMessageStore::global(cx).file.sessions.is_empty());
        });
    }

    #[test]
    fn test_removing_last_grace_window_entry_clears_overdue_grace() {
        let cutoff = utc(12, 0);
        let mut state = ScheduledMessagesState::default();
        state.overdue_grace = Some(OverdueGrace {
            cutoff,
            deadline: cutoff + chrono::Duration::seconds(OVERDUE_GRACE_SECONDS),
        });
        let held = message_at(utc(11, 0), utc(10, 0));
        let future = message_at(utc(14, 0), utc(10, 0));
        state.set.add(held.clone());
        state.set.add(future.clone());

        // Removing an entry outside the grace window keeps the window alive
        // for the entry it still holds.
        state.remove_entry(&future.id);
        assert_eq!(
            state.overdue_grace,
            Some(OverdueGrace {
                cutoff,
                deadline: cutoff + chrono::Duration::seconds(OVERDUE_GRACE_SECONDS),
            })
        );

        // Removing the last grace-held entry clears the window, so stale
        // grace state can never outlive the entries it applied to.
        state.remove_entry(&held.id);
        assert_eq!(state.overdue_grace, None);
    }

    #[test]
    fn test_gc_prunes_sessions_left_empty() {
        let now = utc(12, 0);
        let expired = message_at(
            now - chrono::Duration::days(31),
            now - chrono::Duration::days(31),
        );
        let kept = message_at(now + chrono::Duration::days(1), now);
        let mut sessions = HashMap::default();
        sessions.insert("zed|dead-session".to_string(), vec![expired]);
        sessions.insert("zed|live-session".to_string(), vec![kept.clone()]);
        let file = ScheduledMessagesFile {
            version: SCHEDULED_MESSAGES_FILE_VERSION,
            sessions,
        };

        let collected = gc(file, now);
        assert_eq!(collected.sessions.len(), 1);
        assert_eq!(collected.sessions["zed|live-session"], vec![kept]);
    }
}

/// Integration tests that drive a real thread view (stub agent connection,
/// test window) through restore and the fire routine.
#[cfg(test)]
mod view_tests {
    use super::*;
    use crate::agent_connection_store::AgentConnectionStore;
    use crate::conversation_view::ConversationView;
    use crate::conversation_view::tests::{StubAgentServer, init_test};
    use crate::{Agent, AgentThreadSource};
    use acp_thread::{AgentThreadEntry, StubAgentConnection};
    use agent::ThreadStore;
    use fs::FakeFs;
    use gpui::{App, ReadGlobal as _, TestAppContext, VisualTestContext};
    use project::Project;
    use std::rc::Rc;
    use std::sync::atomic::AtomicUsize;
    use workspace::MultiWorkspace;

    fn init_view_test(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            // A per-test counter makes session ids deterministic ("0",
            // "1", ...), so tests can seed the store for a session key
            // before the view that owns it exists.
            cx.set_global(acp_thread::StubSessionCounter(AtomicUsize::new(0)));
            ScheduledMessageStore::init(cx);
        });
    }

    /// Builds a conversation view around a stub connection and returns its
    /// active thread view. The conversation view must be kept alive by the
    /// caller: it owns the thread view entity.
    async fn setup_thread_view(
        connection: StubAgentConnection,
        cx: &mut TestAppContext,
    ) -> (
        Entity<ConversationView>,
        Entity<ThreadView>,
        &mut VisualTestContext,
    ) {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));
        let conversation_view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    Rc::new(StubAgentServer::new(connection)),
                    connection_store,
                    Agent::Custom { id: "Test".into() },
                    None,
                    None,
                    None,
                    None,
                    None,
                    workspace.downgrade(),
                    project,
                    Some(thread_store),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });
        cx.run_until_parked();
        let thread_view = conversation_view.read_with(cx, |view, _| {
            view.active_thread()
                .expect("stub agent should connect")
                .clone()
        });
        (conversation_view, thread_view, cx)
    }

    fn text_block(text: &str) -> acp::ContentBlock {
        acp::ContentBlock::Text(acp::TextContent::new(text))
    }

    fn scheduled_text_message(
        text: &str,
        fire_at: DateTime<Utc>,
        created_at: DateTime<Utc>,
    ) -> ScheduledMessage {
        ScheduledMessage {
            id: ScheduledMessageId::new(),
            content: vec![text_block(text)],
            fire_at,
            created_at,
        }
    }

    fn queue_texts(view: &ThreadView) -> Vec<String> {
        view.message_queue
            .iter()
            .map(|entry| {
                entry
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect()
            })
            .collect()
    }

    fn user_message_texts(view: &ThreadView, cx: &App) -> Vec<String> {
        view.thread
            .read(cx)
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                AgentThreadEntry::UserMessage(message) => {
                    Some(message.content.to_markdown(cx).to_string())
                }
                _ => None,
            })
            .collect()
    }

    /// Adds a message to both the global store and the view's in-memory
    /// set, mirroring what the scheduling UI will do once it exists. Does
    /// not arm the timer: these tests invoke the fire routine directly.
    fn seed_scheduled_message(
        view: &Entity<ThreadView>,
        message: ScheduledMessage,
        cx: &mut VisualTestContext,
    ) {
        view.update(cx, |view, cx| {
            ScheduledMessageStore::update_global(cx, |store, cx| {
                store.add(view.scheduled.session_key.clone(), message.clone(), cx);
            });
            view.scheduled.set.add(message);
        });
    }

    #[gpui::test]
    async fn test_fire_while_generating_appends_to_queue_tail_without_cancelling(
        cx: &mut TestAppContext,
    ) {
        init_view_test(cx);
        let (_conversation_view, view, cx) =
            setup_thread_view(StubAgentConnection::new(), cx).await;

        // Start a turn that stays in flight: the stub connection resolves a
        // prompt only when explicitly told to end the turn.
        view.update_in(cx, |view, window, cx| {
            view.message_editor
                .update(cx, |editor, cx| editor.set_text("first", window, cx));
            view.send(window, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.thread.read(cx).status(), ThreadStatus::Generating);
        });

        // A message already waiting in the queue makes the tail position
        // observable.
        view.update_in(cx, |view, window, cx| {
            view.add_to_queue(vec![text_block("queued earlier")], Vec::new(), window, cx);
        });

        let now = Utc::now();
        seed_scheduled_message(
            &view,
            scheduled_text_message(
                "scheduled follow-up",
                now - chrono::Duration::seconds(1),
                now,
            ),
            cx,
        );
        view.update_in(cx, |view, window, cx| {
            view.fire_due_scheduled_messages(window, cx);
        });
        cx.run_until_parked();

        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.thread.read(cx).status(),
                ThreadStatus::Generating,
                "firing while generating must not cancel the current turn"
            );
            assert_eq!(
                queue_texts(view),
                vec![
                    "queued earlier".to_string(),
                    "scheduled follow-up".to_string()
                ],
                "the fired message waits at the queue tail"
            );
            assert!(view.scheduled.set.is_empty());
            assert_eq!(user_message_texts(view, cx), vec!["first".to_string()]);
        });
    }

    #[gpui::test]
    async fn test_fire_while_idle_with_empty_queue_starts_new_turn(cx: &mut TestAppContext) {
        init_view_test(cx);
        let (_conversation_view, view, cx) =
            setup_thread_view(StubAgentConnection::new(), cx).await;

        let now = Utc::now();
        seed_scheduled_message(
            &view,
            scheduled_text_message(
                "scheduled kick-off",
                now - chrono::Duration::seconds(1),
                now,
            ),
            cx,
        );

        view.update_in(cx, |view, window, cx| {
            view.fire_due_scheduled_messages(window, cx);
        });
        cx.run_until_parked();

        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.thread.read(cx).status(),
                ThreadStatus::Generating,
                "the fired message itself starts a turn"
            );
            assert_eq!(
                user_message_texts(view, cx),
                vec!["scheduled kick-off".to_string()]
            );
            assert!(
                view.message_queue.is_empty(),
                "the fired message was dispatched, not left queued"
            );
            assert!(view.scheduled.set.is_empty());
        });
    }

    #[gpui::test]
    async fn test_fire_while_idle_with_paused_queue_dispatches_queue_front_first(
        cx: &mut TestAppContext,
    ) {
        init_view_test(cx);
        let (_conversation_view, view, cx) =
            setup_thread_view(StubAgentConnection::new(), cx).await;

        // A turn in flight...
        view.update_in(cx, |view, window, cx| {
            view.message_editor
                .update(cx, |editor, cx| editor.set_text("first", window, cx));
            view.send(window, cx);
        });
        cx.run_until_parked();

        // ...with a follow-up queued, then the user cancels generation: the
        // queue pauses with the follow-up still in it.
        view.update_in(cx, |view, window, cx| {
            view.add_to_queue(vec![text_block("stale queued")], Vec::new(), window, cx);
            view.cancel_generation(cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.thread.read(cx).status(), ThreadStatus::Idle);
            assert_eq!(queue_texts(view), vec!["stale queued".to_string()]);
        });

        let now = Utc::now();
        seed_scheduled_message(
            &view,
            scheduled_text_message("scheduled behind", now - chrono::Duration::seconds(1), now),
            cx,
        );
        view.update_in(cx, |view, window, cx| {
            view.fire_due_scheduled_messages(window, cx);
        });
        cx.run_until_parked();

        view.read_with(cx, |view, cx| {
            assert_eq!(
                user_message_texts(view, cx),
                vec!["first".to_string(), "stale queued".to_string()],
                "the stale queue front is dispatched, not the fired message"
            );
            assert_eq!(
                queue_texts(view),
                vec!["scheduled behind".to_string()],
                "the fired message stays queued behind the drained front"
            );
            assert_eq!(view.thread.read(cx).status(), ThreadStatus::Generating);
        });
    }

    #[gpui::test]
    async fn test_fire_deletes_store_record_before_enqueueing(cx: &mut TestAppContext) {
        init_view_test(cx);
        let (_conversation_view, view, cx) =
            setup_thread_view(StubAgentConnection::new(), cx).await;

        let now = Utc::now();
        let message =
            scheduled_text_message("persist first", now - chrono::Duration::seconds(1), now);
        let id = message.id.clone();
        seed_scheduled_message(&view, message, cx);
        cx.run_until_parked();

        view.update_in(cx, |view, window, cx| {
            view.fire_due_scheduled_messages(window, cx);
        });

        // Synchronously after the fire call: the store record is already
        // gone, but nothing is enqueued yet — the enqueue waits on the
        // delete's persistence task.
        view.read_with(cx, |view, cx| {
            assert!(
                ScheduledMessageStore::global(cx)
                    .messages_for(&view.scheduled.session_key)
                    .is_empty(),
                "the store delete happens before anything is enqueued"
            );
            assert!(
                view.message_queue.is_empty(),
                "the enqueue must wait for the delete to persist"
            );
            assert!(view.scheduled.set.get(&id).is_none());
        });

        cx.run_until_parked();

        // Once the write has landed the message goes out, and a store
        // reloaded from the database no longer has the record.
        view.read_with(cx, |view, cx| {
            assert_eq!(
                user_message_texts(view, cx),
                vec!["persist first".to_string()]
            );
        });
        cx.update(|_, cx| ScheduledMessageStore::init(cx));
        cx.update(|_, cx| {
            assert!(
                ScheduledMessageStore::global(cx)
                    .messages_for(&view.read_with(cx, |view, _| view.scheduled.session_key.clone()))
                    .is_empty()
            );
        });
    }

    #[gpui::test]
    async fn test_restore_populates_state_for_future_entry(cx: &mut TestAppContext) {
        init_view_test(cx);

        // The per-test session counter starts at 0 and the stub server's
        // agent id is "Test", so the first thread's session key is "Test|0".
        let now = Utc::now();
        let future = scheduled_text_message("later", now + chrono::Duration::hours(2), now);
        cx.update(|cx| {
            ScheduledMessageStore::update_global(cx, |store, cx| {
                store.add("Test|0".to_string(), future.clone(), cx);
            })
        });

        let (_conversation_view, view, cx) =
            setup_thread_view(StubAgentConnection::new(), cx).await;

        view.read_with(cx, |view, _| {
            assert_eq!(view.scheduled.session_key, "Test|0");
            assert_eq!(view.scheduled.set.len(), 1);
            assert_eq!(view.scheduled.set.next_fire_at(), Some(future.fire_at));
            assert!(view.scheduled.editors.contains_key(&future.id));
            assert!(view.scheduled.overdue_grace.is_none());
            assert!(view.scheduled.timer.is_some());
        });
    }

    #[gpui::test]
    async fn test_restore_holds_overdue_entry_in_grace_window(cx: &mut TestAppContext) {
        init_view_test(cx);

        let now = Utc::now();
        let overdue = scheduled_text_message(
            "overdue",
            now - chrono::Duration::hours(1),
            now - chrono::Duration::hours(2),
        );
        cx.update(|cx| {
            ScheduledMessageStore::update_global(cx, |store, cx| {
                store.add("Test|0".to_string(), overdue.clone(), cx);
            })
        });

        let (_conversation_view, view, cx) =
            setup_thread_view(StubAgentConnection::new(), cx).await;

        view.read_with(cx, |view, cx| {
            // Held, not fired instantly: still in the runtime set, nothing
            // queued or sent, store record untouched.
            assert_eq!(view.scheduled.set.len(), 1);
            assert!(view.message_queue.is_empty());
            assert_eq!(view.thread.read(cx).entries().len(), 0);
            assert_eq!(
                ScheduledMessageStore::global(cx)
                    .messages_for("Test|0")
                    .len(),
                1
            );

            let grace = view
                .scheduled
                .overdue_grace
                .expect("an overdue entry enters the grace window");
            assert!(grace.deadline > Utc::now());
            assert_eq!(
                grace.deadline,
                grace.cutoff + chrono::Duration::seconds(OVERDUE_GRACE_SECONDS)
            );
            // Its effective fire time is the shared deadline, not the stale
            // fire time: not due at the cutoff, due once the deadline hits.
            assert!(view.scheduled.due_now(grace.cutoff).is_empty());
            assert_eq!(
                view.scheduled.due_now(grace.deadline),
                vec![overdue.id.clone()]
            );
            assert_eq!(view.scheduled.next_wake(), Some(grace.deadline));
            assert!(view.scheduled.timer.is_some());
        });
    }

    /// Seeds like `seed_scheduled_message` but also builds the display
    /// editor and arms the timer — the full runtime state the withdraw /
    /// send-now / delete paths operate on.
    fn seed_scheduled_message_with_runtime_state(
        view: &Entity<ThreadView>,
        message: ScheduledMessage,
        cx: &mut VisualTestContext,
    ) {
        view.update_in(cx, |view, window, cx| {
            ScheduledMessageStore::update_global(cx, |store, cx| {
                store.add(view.scheduled.session_key.clone(), message.clone(), cx);
            });
            let editor = view.build_scheduled_display_editor(message.content.clone(), window, cx);
            view.scheduled.editors.insert(message.id.clone(), editor);
            view.scheduled.set.add(message);
            view.arm_scheduled_message_timer(window, cx);
        });
    }

    fn content_texts(content: &[acp::ContentBlock]) -> Vec<String> {
        content
            .iter()
            .filter_map(|block| match block {
                acp::ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[gpui::test]
    async fn test_withdraw_moves_content_to_empty_main_editor_and_cancels_schedule(
        cx: &mut TestAppContext,
    ) {
        init_view_test(cx);
        let (_conversation_view, view, cx) =
            setup_thread_view(StubAgentConnection::new(), cx).await;

        let now = Utc::now();
        let message =
            scheduled_text_message("come back to me", now + chrono::Duration::hours(1), now);
        let id = message.id.clone();
        seed_scheduled_message_with_runtime_state(&view, message, cx);

        view.update_in(cx, |view, window, cx| {
            view.withdraw_scheduled_message(&id, window, cx);
            // Checked synchronously: this detached view is not in the test
            // window's element tree, so the next draw drops focus from any
            // unpainted handle.
            assert!(
                view.message_editor.focus_handle(cx).is_focused(window),
                "withdraw hands focus to the main editor"
            );
        });
        cx.run_until_parked();

        view.read_with(cx, |view, cx| {
            assert_eq!(view.message_editor.read(cx).text(cx), "come back to me");
            assert!(view.scheduled.set.is_empty());
            assert!(view.scheduled.editors.is_empty());
            assert!(
                view.scheduled.timer.is_none(),
                "no entries left, so no wake-up to arm"
            );
            assert!(view.message_queue.is_empty(), "withdraw never sends");
            assert_eq!(view.thread.read(cx).entries().len(), 0);
            assert!(
                ScheduledMessageStore::global(cx)
                    .messages_for(&view.scheduled.session_key)
                    .is_empty(),
                "the schedule is gone; sending again requires a new time"
            );
        });
    }

    #[gpui::test]
    async fn test_withdraw_appends_after_existing_main_editor_text(cx: &mut TestAppContext) {
        init_view_test(cx);
        let (_conversation_view, view, cx) =
            setup_thread_view(StubAgentConnection::new(), cx).await;

        view.update_in(cx, |view, window, cx| {
            view.message_editor.update(cx, |editor, cx| {
                editor.set_text("draft in progress", window, cx)
            });
        });

        let now = Utc::now();
        let message =
            scheduled_text_message("withdrawn text", now + chrono::Duration::hours(1), now);
        let id = message.id.clone();
        seed_scheduled_message_with_runtime_state(&view, message, cx);

        view.update_in(cx, |view, window, cx| {
            view.withdraw_scheduled_message(&id, window, cx);
        });
        cx.run_until_parked();

        view.read_with(cx, |view, cx| {
            // Same behavior as pulling a queued message back into a
            // non-empty editor: append after a blank line, replace nothing.
            assert_eq!(
                view.message_editor.read(cx).text(cx),
                "draft in progress\n\nwithdrawn text"
            );
            assert!(view.scheduled.set.is_empty());
        });
    }

    #[gpui::test]
    async fn test_send_now_fires_only_the_requested_entry(cx: &mut TestAppContext) {
        init_view_test(cx);
        let (_conversation_view, view, cx) =
            setup_thread_view(StubAgentConnection::new(), cx).await;

        let now = Utc::now();
        let keep = scheduled_text_message("stays scheduled", now + chrono::Duration::hours(2), now);
        let send = scheduled_text_message("goes out now", now + chrono::Duration::hours(1), now);
        seed_scheduled_message_with_runtime_state(&view, keep.clone(), cx);
        seed_scheduled_message_with_runtime_state(&view, send.clone(), cx);

        view.update_in(cx, |view, window, cx| {
            view.send_scheduled_message_now(&send.id, window, cx);
        });

        // Synchronously after the call: the fired entry's store record is
        // already deleted, but nothing is enqueued until that delete has
        // been persisted — the same ordering as a timer-driven fire.
        view.read_with(cx, |view, cx| {
            assert_eq!(
                ScheduledMessageStore::global(cx).messages_for(&view.scheduled.session_key),
                vec![keep.clone()]
            );
            assert!(view.message_queue.is_empty());
            assert!(view.scheduled.set.get(&send.id).is_none());
            assert!(view.scheduled.set.get(&keep.id).is_some());
        });

        cx.run_until_parked();

        view.read_with(cx, |view, cx| {
            assert_eq!(
                user_message_texts(view, cx),
                vec!["goes out now".to_string()],
                "the idle thread dispatches the fired message immediately"
            );
            assert_eq!(view.scheduled.set.len(), 1, "the other entry is untouched");
            assert!(view.scheduled.editors.contains_key(&keep.id));
            assert!(!view.scheduled.editors.contains_key(&send.id));
            assert!(
                view.scheduled.timer.is_some(),
                "the timer is re-armed for the remaining entry"
            );
        });
    }

    #[gpui::test]
    async fn test_delete_removes_entry_without_editor_interaction_or_sending(
        cx: &mut TestAppContext,
    ) {
        init_view_test(cx);
        let (_conversation_view, view, cx) =
            setup_thread_view(StubAgentConnection::new(), cx).await;

        view.update_in(cx, |view, window, cx| {
            view.message_editor
                .update(cx, |editor, cx| editor.set_text("draft stays", window, cx));
        });

        let now = Utc::now();
        let message = scheduled_text_message("discarded", now + chrono::Duration::hours(1), now);
        let id = message.id.clone();
        seed_scheduled_message_with_runtime_state(&view, message, cx);

        view.update_in(cx, |view, window, cx| {
            view.delete_scheduled_message(&id, window, cx);
        });
        cx.run_until_parked();

        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.message_editor.read(cx).text(cx),
                "draft stays",
                "delete never touches the main editor"
            );
            assert!(view.scheduled.set.is_empty());
            assert!(view.scheduled.editors.is_empty());
            assert!(view.scheduled.timer.is_none());
            assert!(view.message_queue.is_empty(), "delete never sends");
            assert_eq!(view.thread.read(cx).entries().len(), 0);
        });

        // The removal reached the database, not just memory.
        cx.update(|_, cx| ScheduledMessageStore::init(cx));
        cx.update(|_, cx| {
            assert!(
                ScheduledMessageStore::global(cx)
                    .messages_for(&view.read_with(cx, |view, _| view.scheduled.session_key.clone()))
                    .is_empty()
            );
        });
    }

    #[gpui::test]
    async fn test_schedule_current_message_moves_editor_content_to_scheduled(
        cx: &mut TestAppContext,
    ) {
        init_view_test(cx);
        let (_conversation_view, view, cx) =
            setup_thread_view(StubAgentConnection::new(), cx).await;

        let fire_at = Utc::now() + chrono::Duration::hours(1);
        view.update_in(cx, |view, window, cx| {
            view.message_editor.update(cx, |editor, cx| {
                editor.set_text("send this later", window, cx)
            });
            view.schedule_current_message(fire_at, window, cx);
        });
        cx.run_until_parked();

        view.read_with(cx, |view, cx| {
            assert!(
                view.message_editor.read(cx).is_empty(cx),
                "scheduling consumes the editor content"
            );
            assert_eq!(view.scheduled.set.len(), 1);
            let entry = view.scheduled.set.iter().next().unwrap();
            assert_eq!(entry.fire_at, fire_at);
            assert_eq!(content_texts(&entry.content), vec!["send this later"]);
            assert!(
                view.scheduled.editors.contains_key(&entry.id),
                "the entry gets a display editor like a restored one"
            );
            assert!(view.scheduled.timer.is_some());
            assert!(view.message_queue.is_empty(), "nothing is sent yet");
            assert_eq!(view.thread.read(cx).entries().len(), 0);
            assert_eq!(
                ScheduledMessageStore::global(cx)
                    .messages_for(&view.scheduled.session_key)
                    .len(),
                1,
                "the entry is persisted"
            );
        });
    }

    #[gpui::test]
    async fn test_schedule_current_message_with_empty_editor_does_nothing(cx: &mut TestAppContext) {
        init_view_test(cx);
        let (_conversation_view, view, cx) =
            setup_thread_view(StubAgentConnection::new(), cx).await;

        let fire_at = Utc::now() + chrono::Duration::hours(1);
        view.update_in(cx, |view, window, cx| {
            view.schedule_current_message(fire_at, window, cx);
        });
        cx.run_until_parked();

        view.read_with(cx, |view, cx| {
            assert!(view.scheduled.set.is_empty());
            assert!(view.scheduled.editors.is_empty());
            assert!(view.scheduled.timer.is_none());
            assert!(
                ScheduledMessageStore::global(cx)
                    .messages_for(&view.scheduled.session_key)
                    .is_empty()
            );
        });
    }
}
