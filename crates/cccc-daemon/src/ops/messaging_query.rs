use cccc_contracts::{DaemonRequest, Event};
use cccc_core::{HomeLayout, ledger};
use serde_json::{Value, json};

use crate::dispatch::{OpError, OpResult, object, required_arg, store, string_arg};

pub fn tail(home: &HomeLayout, request: &DaemonRequest) -> OpResult {
    let limit = integer(request, "limit", 50).min(1000);
    if limit == 0 {
        return object(json!({"events":[],"has_more":false,"count":0}));
    }
    let kind = kind(request, "all");
    let path = ledger_path(home, request)?;
    let (events, has_more) =
        ledger::tail_filtered(&path, limit, kind.filter()).map_err(OpError::io)?;
    result(home, request, Page { events, has_more })
}

pub fn search(home: &HomeLayout, request: &DaemonRequest) -> OpResult {
    let path = ledger_path(home, request)?;
    let query = Query {
        kind: kind(request, "all"),
        text: string_arg(request, "q").unwrap_or_default(),
        by: string_arg(request, "by").unwrap_or_default(),
        before: nonempty(request, "before"),
        after: nonempty(request, "after"),
        limit: integer(request, "limit", 50).clamp(1, 200),
    };
    let page = streaming_page(&path, query)?;
    result(home, request, page)
}

pub fn window(home: &HomeLayout, request: &DaemonRequest) -> OpResult {
    let center_id = required_arg(request, "center")?;
    let path = ledger_path(home, request)?;
    let kind = kind(request, "chat");
    let before_limit = integer(request, "before", 30).min(200);
    let after_limit = integer(request, "after", 30).min(200);
    let mut center = None;
    ledger::visit_oldest_first(&path, |event| {
        if event.id == center_id {
            center = Some(event);
        }
    })
    .map_err(OpError::io)?;
    let center = center
        .ok_or_else(|| OpError::new("event_not_found", format!("event not found: {center_id}")))?;
    if !matches_kind(&center, kind) {
        return Err(OpError::new(
            "invalid_center_kind",
            format!("center event kind must match kind={}", kind.name()),
        ));
    }
    let before = streaming_page(
        &path,
        Query {
            kind,
            before: Some(center_id.clone()),
            limit: before_limit,
            ..Query::default()
        },
    )?;
    let after = streaming_page(
        &path,
        Query {
            kind,
            after: Some(center_id.clone()),
            limit: after_limit,
            ..Query::default()
        },
    )?;
    let center_index = before.events.len();
    let has_more_before = before.has_more;
    let has_more_after = after.has_more;
    let mut combined = before.events;
    combined.push(center);
    combined.extend(after.events);
    let count = combined.len();
    let events = super::messaging_query_status::decorate(home, request, combined)?;
    object(json!({
        "center_id":center_id,
        "center_index":center_index,
        "events":events,
        "has_more_before":has_more_before,
        "has_more_after":has_more_after,
        "count":count,
    }))
}

#[derive(Clone, Copy, Default)]
enum Kind {
    #[default]
    All,
    Chat,
    Notify,
}

impl Kind {
    const fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Chat => "chat",
            Self::Notify => "notify",
        }
    }

    const fn filter(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Chat => Some("chat"),
            Self::Notify => Some("system.notify"),
        }
    }
}

#[derive(Default)]
struct Query {
    kind: Kind,
    text: String,
    by: String,
    before: Option<String>,
    after: Option<String>,
    limit: usize,
}

struct Page {
    events: Vec<Event>,
    has_more: bool,
}

fn streaming_page(path: &std::path::Path, query: Query) -> Result<Page, OpError> {
    let mut before = None;
    let mut after = None;
    let mut position = 0;
    ledger::visit_oldest_first(path, |event| {
        if before.is_none() && query.before.as_deref() == Some(&event.id) {
            before = Some(position);
        }
        if after.is_none() && query.after.as_deref() == Some(&event.id) {
            after = Some(position);
        }
        position += 1;
    })
    .map_err(OpError::io)?;
    for (id, found) in [(&query.before, before), (&query.after, after)] {
        if let Some(id) = id
            && found.is_none()
        {
            return Err(OpError::new(
                "event_not_found",
                format!("event not found: {id}"),
            ));
        }
    }
    let start = after.map_or(0, |n| n + 1);
    let end = before.unwrap_or(position);
    let mut retained = std::collections::VecDeque::new();
    let mut total = 0;
    position = 0;
    let text = query.text.to_lowercase();
    ledger::visit_oldest_first(path, |event| {
        let current = position;
        position += 1;
        if current < start
            || current >= end
            || !matches_kind(&event, query.kind)
            || (!query.by.is_empty() && event.by != query.by)
            || (!text.is_empty()
                && !serde_json::to_string(&event)
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&text))
        {
            return;
        }
        total += 1;
        if query.after.is_some() && retained.len() >= query.limit {
            return;
        }
        retained.push_back(event);
        if retained.len() > query.limit {
            retained.pop_front();
        }
    })
    .map_err(OpError::io)?;
    Ok(Page {
        events: retained.into_iter().collect(),
        has_more: total > query.limit,
    })
}

#[cfg(test)]
fn page(events: &[Event], query: Query) -> Result<Page, OpError> {
    let start = cursor(events, query.after.as_deref())?.map_or(0, |index| index + 1);
    let end = cursor(events, query.before.as_deref())?.unwrap_or(events.len());
    let range = events.get(start.min(end)..end).unwrap_or_default();
    let text = query.text.to_lowercase();
    let mut matches = range
        .iter()
        .filter(|event| matches_kind(event, query.kind))
        .filter(|event| query.by.is_empty() || event.by == query.by)
        .filter(|event| {
            text.is_empty()
                || serde_json::to_string(event)
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&text)
        })
        .cloned()
        .collect::<Vec<_>>();
    let has_more = matches.len() > query.limit;
    if query.after.is_some() {
        matches.truncate(query.limit);
    } else if has_more {
        matches.drain(..matches.len() - query.limit);
    }
    Ok(Page {
        events: matches,
        has_more,
    })
}

#[cfg(test)]
fn cursor(events: &[Event], id: Option<&str>) -> Result<Option<usize>, OpError> {
    let Some(id) = id else { return Ok(None) };
    events
        .iter()
        .position(|event| event.id == id)
        .map(Some)
        .ok_or_else(|| OpError::new("event_not_found", format!("event not found: {id}")))
}

fn matches_kind(event: &Event, kind: Kind) -> bool {
    match kind {
        Kind::All => true,
        Kind::Chat => event.kind == "chat.message",
        Kind::Notify => event.kind == "system.notify",
    }
}

fn ledger_path(home: &HomeLayout, request: &DaemonRequest) -> Result<std::path::PathBuf, OpError> {
    let group = super::messaging::load(home, request)?;
    store(home)?
        .ledger_path(&group.group_id)
        .map_err(OpError::io)
}

fn result(home: &HomeLayout, request: &DaemonRequest, page: Page) -> OpResult {
    let count = page.events.len();
    let events = super::messaging_query_status::decorate(home, request, page.events)?;
    object(json!({"count":count,"events":events,"has_more":page.has_more}))
}

fn kind(request: &DaemonRequest, default: &str) -> Kind {
    match string_arg(request, "kind")
        .unwrap_or_else(|| default.into())
        .to_lowercase()
        .as_str()
    {
        "chat" => Kind::Chat,
        "notify" => Kind::Notify,
        _ => Kind::All,
    }
}

fn nonempty(request: &DaemonRequest, name: &str) -> Option<String> {
    string_arg(request, name).filter(|value| !value.trim().is_empty())
}

fn integer(request: &DaemonRequest, name: &str, default: usize) -> usize {
    request
        .args
        .get(name)
        .and_then(|value| match value {
            Value::Number(number) => number.as_u64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    #[test]
    fn streaming_pagination_matches_indexed_cursor_semantics() {
        let temp = tempfile::tempdir().expect("valid pagination fixture");
        let path = temp.path().join("ledger.jsonl");
        let mut events = (0..15)
            .map(|n| {
                let mut event = Event::new(
                    if n % 3 == 0 {
                        "actor.activity"
                    } else {
                        "chat.message"
                    },
                    "g_test",
                );
                event.id = format!("event-{n}");
                event.by = if n % 2 == 0 { "user" } else { "peer" }.into();
                event.data = json!({"text": format!("needle {n}")})
                    .as_object()
                    .expect("valid pagination fixture")
                    .clone();
                event
            })
            .collect::<Vec<_>>();
        events.push(events[2].clone());
        events.push(events[12].clone());
        let mut file = std::fs::File::create(&path).expect("valid pagination fixture");
        use std::io::Write;
        for event in &events {
            writeln!(
                file,
                "{}",
                serde_json::to_string(event).expect("valid pagination fixture")
            )
            .expect("valid pagination fixture");
        }
        drop(file);
        for before in [None, Some("event-12"), Some("missing")] {
            for after in [None, Some("event-2"), Some("event-14"), Some("missing")] {
                for limit in [0, 1, 4, 20] {
                    let query = || Query {
                        kind: Kind::Chat,
                        text: "needle".into(),
                        by: "peer".into(),
                        before: before.map(str::to_owned),
                        after: after.map(str::to_owned),
                        limit,
                    };
                    let expected = page(&events, query());
                    let actual = streaming_page(&path, query());
                    match (expected, actual) {
                        (Ok(expected), Ok(actual)) => {
                            assert_eq!(expected.events, actual.events);
                            assert_eq!(expected.has_more, actual.has_more);
                        }
                        (Err(_), Err(_)) => {}
                        _ => panic!("cursor result differs"),
                    }
                }
            }
        }
    }
}
