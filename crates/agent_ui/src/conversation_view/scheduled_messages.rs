//! Pure logic for scheduled messages: persisted data types, fire-time text
//! parsing, the per-session in-memory set, and garbage collection.
//!
//! See docs/scheduled-messages.spec.md (§2 data structures, §4.3 GC, §5 time
//! grammar). No GPUI, UI, or storage code lives here; everything is unit
//! testable without an app context.

// Not yet referenced by the thread view; the store and UI integration land in
// follow-up commits. Remove this once the first caller outside tests exists.
#![allow(dead_code)]

use agent_client_protocol::schema::v1 as acp;
use chrono::{DateTime, Local, TimeZone as _, Utc};
use collections::HashMap;
use serde::{Deserialize, Serialize};

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
    pub fn due_entries(&self, now: DateTime<Utc>) -> Vec<ScheduledMessageId> {
        self.entries
            .iter()
            .take_while(|entry| entry.fire_at <= now)
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// The earliest fire time, used to re-arm the wake-up timer after any
    /// change to the set.
    pub fn next_fire_at(&self) -> Option<DateTime<Utc>> {
        self.entries.first().map(|entry| entry.fire_at)
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
    use chrono::{DateTime, Local, TimeZone as _, Utc};
    use collections::HashMap;

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
