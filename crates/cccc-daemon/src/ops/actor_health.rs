//! Actor health on `actor_list` rows, and an `actor.health` event when an
//! Actor's bucket changes.
//!
//! The facts are counts over the last 48 hours: down episodes (the Actor's
//! process exited without being stopped) and, for Actors whose managed runtime
//! reports its working state, ignored deliveries. They live in memory, one
//! small window per Actor, fed by the events appended to each Group's ledger:
//! the activity publisher's tick follows the ledger and reads only what was
//! appended since its last tick. At daemon start the window is rebuilt from the
//! end of the active ledger file, at most [`REBUILD_TAIL_BYTES`] and 48 hours.
//! Neither a tick nor an `actor_list` call scans the ledger.

use cccc_contracts::{Actor, Event};
use cccc_core::ledger::{self, LedgerFollower};
use cccc_core::{GroupDoc, GroupStore};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{LazyLock, Mutex, PoisonError};

/// Facts count what happened in the last 48 hours: long enough to see a
/// pattern across a working day and a night, short enough to forgive.
pub(crate) const WINDOW_SECONDS: i64 = 48 * 60 * 60;
/// Each fact keeps at most this many timestamps per Actor, so a count stops
/// at 64; past that the bucket cannot get any worse.
pub(crate) const MAX_STAMPS: usize = 64;
/// At daemon start at most this much of the end of each Group's active ledger
/// file is read, one line at a time. A busy Group writes about 16 MB in 48
/// hours, so this covers the window for it.
pub(crate) const REBUILD_TAIL_BYTES: u64 = 32 * 1024 * 1024;
/// One down episode or ignored delivery in the window makes an Actor
/// `degraded`: worth a look, not yet a reason to route work elsewhere.
pub(crate) const DEGRADED_AT: usize = 1;
/// Three make it `unhealthy`: a pattern, not a one-off.
pub(crate) const UNHEALTHY_AT: usize = 3;
/// Default of the `delivery.ignored_delivery_after_seconds` setting: an idle
/// Actor that neither starts work nor writes anything this long after a
/// delivery has ignored it.
pub(crate) const DEFAULT_IGNORED_AFTER_SECONDS: i64 = 30 * 60;

#[derive(Debug, Default)]
struct Facts {
    downs: VecDeque<i64>,
    ignored: VecDeque<i64>,
    /// Deliveries accepted while the Actor reported idle, not yet answered.
    pending: VecDeque<i64>,
    /// The last messages counted as delivered, so a retry counts once.
    delivered: VecDeque<String>,
    /// The Actor's last published working state, if its runtime reports one.
    state: Option<String>,
    /// The bucket last recorded in an `actor.health` event.
    published: Option<String>,
}

impl Facts {
    /// Move to `now`: unanswered deliveries older than `ignored_after` become
    /// ignored, and stamps that left the window are dropped.
    fn advance(&mut self, now: i64, ignored_after: i64) {
        if ignored_after <= 0 {
            self.pending.clear();
        }
        while let Some(&delivered) = self.pending.front() {
            if delivered + ignored_after > now {
                break;
            }
            self.pending.pop_front();
            push(&mut self.ignored, delivered);
        }
        let floor = now - WINDOW_SECONDS;
        for stamps in [&mut self.downs, &mut self.ignored] {
            while stamps.front().is_some_and(|&stamp| stamp < floor) {
                stamps.pop_front();
            }
        }
    }

    /// Counts at `now` without changing anything (for `actor_list`).
    fn counts(&self, now: i64, ignored_after: i64) -> (usize, usize) {
        let floor = now - WINDOW_SECONDS;
        let downs = self.downs.iter().filter(|&&stamp| stamp >= floor).count();
        let expired = if ignored_after > 0 {
            self.pending
                .iter()
                .filter(|&&delivered| delivered + ignored_after <= now && delivered >= floor)
                .count()
        } else {
            0
        };
        let ignored = self.ignored.iter().filter(|&&stamp| stamp >= floor).count() + expired;
        (downs, ignored.min(MAX_STAMPS))
    }

    fn answered(&mut self) {
        self.pending.clear();
    }

    /// Whether this is the first delivery of the message `source`.
    fn first_delivery(&mut self, source: &str) -> bool {
        if self.delivered.iter().any(|seen| seen == source) {
            return false;
        }
        if self.delivered.len() == MAX_STAMPS {
            self.delivered.pop_front();
        }
        self.delivered.push_back(source.to_owned());
        true
    }
}

fn push(stamps: &mut VecDeque<i64>, stamp: i64) {
    if stamps.len() == MAX_STAMPS {
        stamps.pop_front();
    }
    stamps.push_back(stamp);
}

struct Book {
    follower: LedgerFollower,
    actors: BTreeMap<String, Facts>,
}

impl Book {
    fn facts(&mut self, actor_id: &str) -> &mut Facts {
        self.actors.entry(actor_id.to_owned()).or_default()
    }

    /// Fold one ledger event into the facts.
    fn feed(&mut self, event: &Event, ignored_after: i64) {
        let Some(ts) = unix_seconds(&event.ts) else {
            return;
        };
        let data = &event.data;
        match event.kind.as_str() {
            "actor.add" => {
                if let Some(actor_id) = data
                    .get("actor")
                    .and_then(|actor| actor.get("id"))
                    .and_then(Value::as_str)
                {
                    self.actors.insert(actor_id.to_owned(), Facts::default());
                }
            }
            "actor.remove" => {
                if let Some(actor_id) = text(data, "actor_id") {
                    self.actors.remove(actor_id);
                }
            }
            "actor.stop" if text(data, "reason") == Some("process_exit") => {
                if let Some(actor_id) = text(data, "actor_id") {
                    let facts = self.facts(actor_id);
                    facts.advance(ts, ignored_after);
                    push(&mut facts.downs, ts);
                    facts.answered();
                    facts.state = None;
                }
            }
            // `ambiguous`: the runtime did not confirm the delivery in time. It
            // counts like an accepted one: if the message did arrive and the
            // Actor acts on it, the change of state answers it. A retry of the
            // same message counts once.
            "runtime.delivery" if matches!(text(data, "state"), Some("accepted" | "ambiguous")) => {
                if let Some(actor_id) = text(data, "actor_id") {
                    let source = text(data, "source_event_id").unwrap_or_default();
                    let facts = self.facts(actor_id);
                    facts.advance(ts, ignored_after);
                    // Only an Actor that positively reports idle can ignore it.
                    if facts.first_delivery(source)
                        && ignored_after > 0
                        && facts.state.as_deref() == Some("idle")
                    {
                        push(&mut facts.pending, ts);
                    }
                }
            }
            "actor.activity" => {
                for payload in data
                    .get("actors")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(actor_id) = payload.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let running = payload.get("running").and_then(Value::as_bool) == Some(true);
                    let state = payload
                        .get("effective_working_state")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let tracked = running
                        && payload
                            .get("effective_working_reason")
                            .and_then(Value::as_str)
                            .is_some_and(super::working_state::reports_state);
                    let state = state.filter(|_| tracked);
                    let facts = self.facts(actor_id);
                    facts.advance(ts, ignored_after);
                    // Any change of state is the Actor's own doing; an Actor
                    // whose state is unknown or that stopped counts nothing.
                    if state.is_none() || facts.state != state {
                        facts.answered();
                    }
                    facts.state = state;
                }
            }
            "actor.health" => {
                if let (Some(actor_id), Some(bucket)) =
                    (text(data, "actor_id"), text(data, "health"))
                {
                    self.facts(actor_id).published = Some(bucket.to_owned());
                }
            }
            _ => {}
        }
        // Anything an Actor writes itself (a message, a read, a sync) answers
        // what was delivered to it.
        if !matches!(event.by.as_str(), "" | "system" | "user")
            && let Some(facts) = self.actors.get_mut(&event.by)
        {
            facts.advance(ts, ignored_after);
            facts.answered();
        }
    }
}

static BOOKS: LazyLock<Mutex<BTreeMap<String, Book>>> = LazyLock::new(Mutex::default);

fn books() -> std::sync::MutexGuard<'static, BTreeMap<String, Book>> {
    BOOKS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The seconds after which an unanswered delivery counts as ignored; 0 turns
/// the count off.
pub(crate) fn ignored_after_seconds(group: &GroupDoc) -> i64 {
    group
        .extra
        .get("delivery")
        .and_then(Value::as_object)
        .and_then(|delivery| delivery.get("ignored_delivery_after_seconds"))
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_IGNORED_AFTER_SECONDS)
        .max(0)
}

/// An Actor is expected to run when it is enabled in a running Group.
fn expected(group: &GroupDoc, actor: &Actor) -> bool {
    group.running && actor.enabled
}

/// The coarse bucket: `down` (expected to run but not running), else by the
/// number of problems in the window.
pub(crate) fn bucket(down: bool, downs: usize, ignored: usize) -> &'static str {
    let problems = downs + ignored;
    if down {
        "down"
    } else if problems >= UNHEALTHY_AT {
        "unhealthy"
    } else if problems >= DEGRADED_AT {
        "degraded"
    } else {
        "healthy"
    }
}

/// The counts shared by actor rows and `actor.health` events. The ignored
/// count is shown where it can be known: the runtime reports its working
/// state now (and the setting is on), or deliveries were found ignored while
/// it did. Otherwise it is absent, never a misleading 0.
fn count_fields(downs: usize, ignored: usize, reports_state: bool) -> Map<String, Value> {
    let mut fields = Map::from_iter([("down_episodes_48h".into(), json!(downs))]);
    if reports_state || ignored > 0 {
        fields.insert("ignored_deliveries_48h".into(), json!(ignored));
    }
    fields
}

/// The health fields of one `actor_list` row. `working_reason` is the row's
/// `effective_working_reason`.
pub(crate) fn row_fields(
    group: &GroupDoc,
    actor: &Actor,
    running: bool,
    working_reason: Option<&str>,
) -> Map<String, Value> {
    let ignored_after = ignored_after_seconds(group);
    let now = chrono::Utc::now().timestamp();
    let (downs, ignored) = books()
        .get(&group.group_id)
        .and_then(|book| book.actors.get(&actor.id))
        .map_or((0, 0), |facts| facts.counts(now, ignored_after));
    let reports_state = ignored_after > 0
        && running
        && working_reason.is_some_and(super::working_state::reports_state);
    let mut fields = count_fields(downs, ignored, reports_state);
    if expected(group, actor) {
        fields.insert("health".into(), json!(bucket(!running, downs, ignored)));
    }
    fields
}

/// One tick for one Group, from the activity publisher: fold in the events
/// appended since the last tick, then record an `actor.health` event for each
/// Actor whose bucket changed. `emit` is false while the daemon is still
/// restoring runtimes or another operation holds the Group, so a transient
/// state is never recorded.
pub(crate) fn tick_group(
    store: &GroupStore,
    group: &GroupDoc,
    current: &BTreeMap<String, (bool, bool)>,
    emit: bool,
) {
    let Ok(path) = store.ledger_path(&group.group_id) else {
        return;
    };
    let ignored_after = ignored_after_seconds(group);
    // File reads and appends happen outside the lock that actor_list reads.
    if !books().contains_key(&group.group_id) {
        match open_book(&path, ignored_after) {
            Ok(book) => {
                books().entry(group.group_id.clone()).or_insert(book);
            }
            Err(error) => {
                tracing::warn!(%error, group_id = %group.group_id, "actor health rebuild failed");
                return;
            }
        }
    }
    let Some(mut follower) = books()
        .get_mut(&group.group_id)
        .map(|book| std::mem::take(&mut book.follower))
    else {
        return;
    };
    let appended = follower.poll(&path);
    let now = chrono::Utc::now().timestamp();
    let changes = {
        let mut books = books();
        let Some(book) = books.get_mut(&group.group_id) else {
            return;
        };
        book.follower = follower;
        match appended {
            Ok(events) => {
                for event in &events {
                    book.feed(event, ignored_after);
                }
            }
            Err(error) => {
                tracing::warn!(%error, group_id = %group.group_id, "actor health follow failed");
            }
        }
        book.actors
            .retain(|actor_id, _| group.actors.iter().any(|actor| &actor.id == actor_id));
        let mut changes = Vec::new();
        for actor in group
            .actors
            .iter()
            .filter(|actor| actor.internal_kind.is_none())
        {
            let facts = book.facts(&actor.id);
            facts.advance(now, ignored_after);
            if !emit || !expected(group, actor) {
                continue;
            }
            let (running, reports_state) =
                current.get(&actor.id).copied().unwrap_or((false, false));
            let (downs, ignored) = facts.counts(now, ignored_after);
            let health = bucket(!running, downs, ignored);
            let previous = facts.published.as_deref().unwrap_or("healthy");
            if health == previous {
                continue;
            }
            let mut data = Map::from_iter([
                ("actor_id".into(), json!(actor.id)),
                ("health".into(), json!(health)),
                ("previous".into(), json!(previous)),
            ]);
            data.extend(count_fields(
                downs,
                ignored,
                reports_state && ignored_after > 0,
            ));
            let mut event = Event::new("actor.health", &group.group_id);
            event.by = "system".into();
            event.data = data;
            changes.push((actor.id.clone(), health, event));
        }
        changes
    };
    for (actor_id, health, event) in changes {
        match ledger::append(&path, &event) {
            Ok(()) => {
                if let Some(facts) = books()
                    .get_mut(&group.group_id)
                    .and_then(|book| book.actors.get_mut(&actor_id))
                {
                    facts.published = Some(health.to_owned());
                }
            }
            Err(error) => tracing::warn!(
                %error,
                group_id = %group.group_id,
                actor_id = %actor_id,
                "failed to append actor.health"
            ),
        }
    }
}

/// Forget Groups that no longer exist.
pub(crate) fn retain_groups(known: &std::collections::BTreeSet<String>) {
    books().retain(|group_id, _| known.contains(group_id));
}

/// Start following a Group's ledger at its end, and rebuild the window from
/// the tail of the active file up to the event the follower starts after.
fn open_book(path: &Path, ignored_after: i64) -> io::Result<Book> {
    let (follower, cursor) = LedgerFollower::at_end(path)?;
    let mut book = Book {
        follower,
        actors: BTreeMap::new(),
    };
    let now = chrono::Utc::now().timestamp();
    rebuild(path, &mut book, cursor.as_deref(), now, ignored_after)?;
    Ok(book)
}

fn rebuild(
    path: &Path,
    book: &mut Book,
    cursor: Option<&str>,
    now: i64,
    ignored_after: i64,
) -> io::Result<()> {
    let Some(cursor) = cursor else {
        return Ok(());
    };
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let len = file.metadata()?.len();
    let start = len.saturating_sub(REBUILD_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file.take(len - start));
    let mut line = Vec::new();
    if start > 0 {
        // The first line read from the middle of the file is partial.
        reader.read_until(b'\n', &mut line)?;
    }
    let floor = now - WINDOW_SECONDS;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let Ok(event) = serde_json::from_slice::<Event>(&line) else {
            continue;
        };
        if unix_seconds(&event.ts).is_some_and(|ts| ts >= floor) {
            book.feed(&event, ignored_after);
        }
        if event.id == cursor {
            break;
        }
    }
    for facts in book.actors.values_mut() {
        facts.advance(now, ignored_after);
    }
    Ok(())
}

fn text<'a>(data: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    data.get(key).and_then(Value::as_str)
}

fn unix_seconds(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|time| time.timestamp())
}

/// Drop a Group's window, as a daemon restart does.
#[cfg(test)]
pub(super) fn forget(group_id: &str) {
    books().remove(group_id);
}

#[cfg(test)]
pub(super) fn replay(
    path: &Path,
    now: i64,
    ignored_after: i64,
) -> BTreeMap<String, (usize, usize)> {
    let mut book = Book {
        follower: LedgerFollower::default(),
        actors: BTreeMap::new(),
    };
    let last = ledger::tail(path, 1)
        .ok()
        .and_then(|events| events.last().map(|event| event.id.clone()));
    rebuild(path, &mut book, last.as_deref(), now, ignored_after).expect("rebuild");
    book.actors
        .iter()
        .map(|(actor_id, facts)| (actor_id.clone(), facts.counts(now, ignored_after)))
        .collect()
}
