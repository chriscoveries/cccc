//! Bounded projection of the context snapshot.
//!
//! A group with ten thousand cards projects to a multi-megabyte answer; a
//! single `cccc_context_get` then fails the client's transport and the client
//! retries until the session corrupts (owner spec dd17cbba, items 1-2). The
//! daemon therefore bounds every projection by default:
//!
//! * `limit` caps how many board/task items one answer carries (default 100;
//!   `0` is rejected, `unbounded: true` is the only bypass),
//! * `max_board_items` (group config) caps the same collection server-side,
//! * `state_max_bytes` (group config, default 256 KiB) caps the serialized
//!   answer, shrinking the page until it fits,
//! * `include` selects `coordination`, `board`, `actors`,
//! * `summary` keeps counts and the brief but no item lists,
//! * `cursor` resumes from the previous `next_cursor`.
//!
//! Truncation is reported with `truncated: true` plus `next_cursor`; the op
//! never fails because an answer is large. Callers that genuinely need the
//! whole snapshot (the web UI's full-board views) pass `unbounded: true`.
//!
//! The cap is enforced, not estimated: a page that would overflow is shrunk,
//! an item that cannot fit is delivered as an `oversized` stub so the cursor
//! still advances, and the envelope itself is trimmed (`envelope_trimmed`)
//! before the walk can stall.

use cccc_contracts::DaemonRequest;
use cccc_core::GroupDoc;
use cccc_core::context::ContextDoc;
use serde_json::{Map, Value, json};

use crate::dispatch::{OpError, bool_arg, string_arg};

pub(super) const DEFAULT_LIMIT: usize = 100;
pub(super) const MAX_LIMIT: usize = 1_000;
pub(super) const DEFAULT_MAX_BOARD_ITEMS: usize = 100;
pub(super) const DEFAULT_STATE_MAX_BYTES: usize = 256 * 1024;
/// Reserve kept free of the byte cap so at least one item (a stub, at worst)
/// always fits on a page.
const ITEM_RESERVE: usize = 256;
/// Below this the mandatory envelope cannot be made to fit, so the cap is not
/// enforceable and the answer reports `paging.stalled` instead.
const MIN_ENVELOPE_BYTES: usize = 1024;
/// `recent_decisions`/`recent_handoffs` entries kept on a bounded answer.
const RECENT_MAX: usize = 20;

const CURSOR_PREFIX: &str = "v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Bounds {
    pub limit: usize,
    pub max_board_items: usize,
    pub max_bytes: usize,
    pub include_coordination: bool,
    pub include_board: bool,
    pub include_actors: bool,
    pub include_archived: bool,
    pub summary: bool,
    pub task_offset: usize,
    pub actor_offset: usize,
}

impl Bounds {
    /// `None` means the caller asked for the complete, unbounded snapshot.
    pub(super) fn from_request(
        request: &DaemonRequest,
        group: &GroupDoc,
        detail: &str,
    ) -> Result<Option<Self>, OpError> {
        if bool_arg(request, "unbounded", false) {
            return Ok(None);
        }
        let limit = match request.args.get("limit") {
            None | Some(Value::Null) => DEFAULT_LIMIT,
            Some(value) => {
                let Some(raw) = value.as_u64() else {
                    return Err(OpError::new(
                        "invalid_args",
                        "limit must be a non-negative integer",
                    ));
                };
                if raw == 0 {
                    // `limit: 0` used to mean "the whole snapshot", which is
                    // the multi-megabyte answer this op exists to prevent.
                    // The bypass is `unbounded: true`, and it is explicit.
                    return Err(OpError::new(
                        "invalid_args",
                        "limit must be at least 1; pass unbounded=true for the complete snapshot",
                    ));
                }
                usize::try_from(raw).unwrap_or(MAX_LIMIT).min(MAX_LIMIT)
            }
        };
        let max_board_items = config_usize(group, "max_board_items")
            .unwrap_or(DEFAULT_MAX_BOARD_ITEMS)
            .clamp(1, MAX_LIMIT);
        // The floor is the size below which no envelope can be made to fit:
        // a smaller cap could not be honoured, only reported as stalled.
        let max_bytes = config_usize(group, "state_max_bytes")
            .unwrap_or(DEFAULT_STATE_MAX_BYTES)
            .max(MIN_ENVELOPE_BYTES);
        let (include_coordination, include_board, include_actors) = include_set(request, detail)?;
        let (task_offset, actor_offset) = match string_arg(request, "cursor") {
            Some(cursor) if !cursor.trim().is_empty() => parse_cursor(cursor.trim())?,
            _ => (0, 0),
        };
        Ok(Some(Self {
            limit,
            max_board_items,
            max_bytes,
            include_coordination,
            include_board,
            include_actors,
            // Archived cards are hidden from a page by default, the same way
            // the MCP layer hides them: filtering here keeps the page full and
            // makes `paging.task_total` describe the cards the caller can see.
            include_archived: bool_arg(request, "include_archived", false),
            summary: bool_arg(request, "summary", false),
            task_offset,
            actor_offset,
        }))
    }

    fn page_size(&self) -> usize {
        self.limit.min(self.max_board_items)
    }
}

fn include_set(request: &DaemonRequest, detail: &str) -> Result<(bool, bool, bool), OpError> {
    // An overview has no board to page; asking for actors and coordination is
    // what its consumers read. Everything else defaults to all three.
    let default = if detail == "overview" {
        (true, false, true)
    } else {
        (true, true, true)
    };
    let Some(value) = request.args.get("include") else {
        return Ok(default);
    };
    if value.is_null() {
        return Ok(default);
    }
    let entries: Vec<String> = match value {
        Value::String(text) => text
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_owned)
            .collect(),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| OpError::new("invalid_args", "include entries must be strings"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(OpError::new(
                "invalid_args",
                "include must be a string or an array of strings",
            ));
        }
    };
    if entries.is_empty() {
        return Err(OpError::new(
            "invalid_args",
            "include must name at least one of coordination, board, actors",
        ));
    }
    let (mut coordination, mut board, mut actors) = (false, false, false);
    for entry in entries {
        match entry.to_ascii_lowercase().as_str() {
            "coordination" => coordination = true,
            "board" => board = true,
            "actors" | "actor" => actors = true,
            _ => {
                return Err(OpError::new(
                    "invalid_args",
                    format!("unknown include section: {entry} (use coordination, board, actors)"),
                ));
            }
        }
    }
    Ok((coordination, board, actors))
}

fn parse_cursor(cursor: &str) -> Result<(usize, usize), OpError> {
    let (prefix, tasks, actors) = match cursor.split('.').collect::<Vec<_>>().as_slice() {
        [prefix, tasks, actors] => (*prefix, *tasks, *actors),
        _ => return Err(malformed_cursor(cursor)),
    };
    if prefix != CURSOR_PREFIX {
        return Err(malformed_cursor(cursor));
    }
    match (tasks.parse::<usize>(), actors.parse::<usize>()) {
        (Ok(tasks), Ok(actors)) => Ok((tasks, actors)),
        _ => Err(malformed_cursor(cursor)),
    }
}

fn malformed_cursor(cursor: &str) -> OpError {
    OpError::new(
        "invalid_args",
        format!("cursor is not a next_cursor value: {cursor}"),
    )
}

/// Group caps live in group.yaml. They are read from the top level or from the
/// former `settings` block so existing groups can adopt them without a schema
/// migration. Unset, zero or non-numeric values fall back to the defaults.
fn config_usize(group: &GroupDoc, key: &str) -> Option<usize> {
    group
        .extra
        .get(key)
        .or_else(|| {
            group
                .extra
                .get("settings")
                .and_then(Value::as_object)
                .and_then(|settings| settings.get(key))
        })
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
}

pub(super) fn next_cursor(task_offset: usize, actor_offset: usize) -> String {
    format!("{CURSOR_PREFIX}.{task_offset}.{actor_offset}")
}

/// One page of the bounded projection.
///
/// Invariants fixed after the T771 integration review (2026-10-19):
///
/// * the serialized answer never exceeds `max_bytes`: the mandatory envelope
///   is trimmed first (named in `paging.envelope_trimmed`) and keeps a reserve
///   for one item, so a page never comes back empty while items remain;
/// * an item too large for what is left of the budget is delivered as a stub
///   (`{"id": ..., "oversized": true}`, id also in `paging.oversized`) rather
///   than admitted and then dropped back to the same offset;
/// * a truncated answer always advances the cursor, so every walk terminates.
///   If nothing can advance (unreachable while `max_bytes` is at least
///   [`MIN_ENVELOPE_BYTES`]) the answer reports `paging.stalled` and a null
///   cursor instead of repeating itself forever.
pub(super) fn project(
    document: &ContextDoc,
    version: &str,
    detail: &str,
    bounds: &Bounds,
) -> Value {
    // `overview` is the same bounded machinery without the task lists: the
    // overview document is loaded without tasks, and its consumers want the
    // coordination brief plus actor states, not board counts.
    let overview = detail == "overview";
    let tasks: Vec<&Map<String, Value>> = if overview {
        Vec::new()
    } else {
        document
            .tasks
            .iter()
            .filter(|task| bounds.include_archived || status(task) != "archived")
            .collect()
    };
    let total_tasks = tasks.len();
    let actors: Vec<(&String, &Map<String, Value>)> = document.agent_states.iter().collect();
    let total_actors = actors.len();
    let page_size = bounds.page_size();
    let budget = bounds.max_bytes;

    // The envelope is mandatory, so it is trimmed before any item is placed:
    // optional sections go in order of least loss, and the reserve keeps room
    // for one stub.
    let mut envelope = envelope(document, version, detail, bounds);
    let mut trimmed: Vec<&'static str> = Vec::new();
    for part in ["attention", "tasks_summary", "meta"] {
        if serialized_len(&Value::Object(envelope.clone())) + ITEM_RESERVE <= budget {
            break;
        }
        if envelope.remove(part).is_some() {
            trimmed.push(part);
        }
    }
    for (part, name) in [
        ("recent_handoffs", "coordination.recent_handoffs"),
        ("recent_decisions", "coordination.recent_decisions"),
        ("brief", "coordination.brief"),
    ] {
        if serialized_len(&Value::Object(envelope.clone())) + ITEM_RESERVE <= budget {
            break;
        }
        let removed = envelope
            .get_mut("coordination")
            .and_then(Value::as_object_mut)
            .and_then(|coordination| coordination.remove(part))
            .is_some();
        if removed {
            trimmed.push(name);
        }
    }

    let mut used = serialized_len(&Value::Object(envelope.clone()));
    let mut task_end = bounds.task_offset.min(total_tasks);
    let mut page: Vec<Value> = Vec::new();
    let mut actor_end = bounds.actor_offset.min(total_actors);
    let mut actor_page: Vec<Value> = Vec::new();
    let mut oversized: Vec<String> = Vec::new();
    let mut detail_reduced = false;

    if !bounds.summary {
        if !overview && (bounds.include_coordination || bounds.include_board) {
            while page.len() < page_size && task_end < total_tasks {
                let task = tasks[task_end];
                let id = task_id(task);
                let mut rendered = render_task(task, detail == "full");
                let mut size = serialized_len(&rendered) + 1;
                if used.saturating_add(size) > budget {
                    if !page.is_empty() {
                        break;
                    }
                    // The head of the page does not fit: shrink the item, not
                    // the progress. Lean first; a stub last.
                    if detail == "full" {
                        let lean = render_task(task, false);
                        let lean_size = serialized_len(&lean) + 1;
                        if used.saturating_add(lean_size) <= budget {
                            rendered = lean;
                            size = lean_size;
                            detail_reduced = true;
                        }
                    }
                    if used.saturating_add(size) > budget {
                        // A stub, not a silent overshoot: the id travels both
                        // in the page (so the caller knows the card exists) and
                        // in `paging.oversized` (so it knows the card's body was
                        // withheld).
                        rendered = json!({"id": &id, "oversized": true});
                        size = serialized_len(&rendered) + 1;
                        oversized.push(id);
                    }
                }
                used = used.saturating_add(size);
                page.push(rendered);
                task_end += 1;
            }
        }
        if bounds.include_actors {
            while actor_page.len() < page_size && actor_end < total_actors {
                let (actor_id, state) = actors[actor_end];
                let mut state = state.clone();
                state
                    .entry("id")
                    .or_insert_with(|| Value::String(actor_id.clone()));
                let mut rendered = Value::Object(state);
                let mut size = serialized_len(&rendered) + 1;
                if used.saturating_add(size) > budget {
                    if !actor_page.is_empty() {
                        break;
                    }
                    rendered = json!({"id": actor_id, "oversized": true});
                    size = serialized_len(&rendered) + 1;
                    oversized.push(actor_id.clone());
                }
                used = used.saturating_add(size);
                actor_page.push(rendered);
                actor_end += 1;
            }
        }
    }

    // The estimate above ignores separators and the closing braces of the
    // lists, so verify the real serialization and drop trailing items until the
    // answer genuinely fits the cap.
    loop {
        let value = assemble(
            envelope.clone(),
            detail,
            bounds,
            &page,
            &actor_page,
            task_end,
            actor_end,
            total_tasks,
            total_actors,
            &trimmed,
            &oversized,
            detail_reduced,
            recent_capped(document, detail),
        );
        if serialized_len(&value) <= budget {
            return value;
        }
        if !page.is_empty() {
            page.pop();
            task_end -= 1;
        } else if !actor_page.is_empty() {
            actor_page.pop();
            actor_end -= 1;
        } else {
            // Nothing left to drop: the envelope alone is over the cap. Return
            // it (the op never errors on size) with `paging.stalled` set so the
            // caller is not left looping on a repeated cursor.
            return value;
        }
    }
}

fn task_id(task: &Map<String, Value>) -> String {
    task.get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// True when `recent_decisions`/`recent_handoffs` were cut to [`RECENT_MAX`].
fn recent_capped(document: &ContextDoc, detail: &str) -> bool {
    detail != "summary"
        && ["recent_decisions", "recent_handoffs"].iter().any(|key| {
            document
                .coordination
                .get(*key)
                .and_then(Value::as_array)
                .is_some_and(|items| items.len() > RECENT_MAX)
        })
}

#[allow(clippy::too_many_arguments)]
fn assemble(
    mut value: Map<String, Value>,
    detail: &str,
    bounds: &Bounds,
    page: &[Value],
    actor_page: &[Value],
    task_end: usize,
    actor_end: usize,
    total_tasks: usize,
    total_actors: usize,
    trimmed: &[&str],
    oversized: &[String],
    detail_reduced: bool,
    recent_capped: bool,
) -> Value {
    let overview = detail == "overview";
    if bounds.include_coordination && !bounds.summary && !overview {
        if let Some(coordination) = value.get_mut("coordination").and_then(Value::as_object_mut) {
            coordination.insert("tasks".into(), Value::Array(page.to_vec()));
            coordination.insert(
                "tasks_paging".into(),
                json!({
                    "task_count": page.len(),
                    "task_total": total_tasks,
                    "detail": detail,
                }),
            );
        }
    }
    if bounds.include_board && !bounds.summary && !overview {
        value.insert("board".into(), board(page));
    }
    if bounds.include_actors {
        value.insert("agent_states".into(), Value::Array(actor_page.to_vec()));
    }
    // A section the caller did not ask for is complete by definition: it must
    // not keep the cursor alive, or an actors-only page would never terminate.
    let tasks_requested = !overview && (bounds.include_coordination || bounds.include_board);
    let tasks_done = bounds.summary || !tasks_requested || task_end >= total_tasks;
    let actors_done = bounds.summary || !bounds.include_actors || actor_end >= total_actors;
    let truncated = !(tasks_done && actors_done);
    let task_offset = bounds.task_offset.min(total_tasks);
    let actor_offset = bounds.actor_offset.min(total_actors);
    // Forward progress is the cursor's contract: it is only handed back when
    // this answer actually moved past something.
    let advanced = task_end > task_offset || actor_end > actor_offset;
    value.insert("truncated".into(), Value::Bool(truncated));
    value.insert(
        "next_cursor".into(),
        if !truncated || !advanced {
            Value::Null
        } else {
            Value::String(next_cursor(task_end, actor_end))
        },
    );
    // An overview has no tasks: reporting `task_total: 0` there would read as
    // an empty board instead of "not loaded", so task counts are omitted.
    let mut paging = json!({
        "limit": bounds.page_size(),
        "actor_offset": actor_offset,
        "actor_count": actor_page.len(),
        "actor_total": total_actors,
        "max_bytes": bounds.max_bytes,
        "oversized": oversized,
        "detail_reduced": detail_reduced,
        "recent_capped": recent_capped,
        "envelope_trimmed": trimmed,
        "stalled": truncated && !advanced,
    });
    if !overview {
        paging["task_offset"] = json!(task_offset);
        paging["task_count"] = json!(page.len());
        paging["task_total"] = json!(total_tasks);
    }
    value.insert("paging".into(), paging);
    Value::Object(value)
}

fn envelope(
    document: &ContextDoc,
    version: &str,
    detail: &str,
    bounds: &Bounds,
) -> Map<String, Value> {
    let mut value = Map::new();
    value.insert("version".into(), Value::String(version.to_owned()));
    value.insert(
        "tasks_version".into(),
        Value::String(format!("tasksv:{}", document.tasks_revision)),
    );
    // An overview document carries no tasks: board counts there would read as
    // an empty board rather than as "not loaded".
    if detail != "overview" {
        value.insert("tasks_summary".into(), task_summary(&document.tasks));
        value.insert("attention".into(), attention(&document.tasks));
    }
    if bounds.include_coordination {
        let mut coordination = Map::new();
        coordination.insert(
            "brief".into(),
            document
                .coordination
                .get("brief")
                .cloned()
                .unwrap_or_else(|| json!({})),
        );
        if detail != "summary" {
            for key in ["recent_decisions", "recent_handoffs"] {
                let mut items = document
                    .coordination
                    .get(key)
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let total = items.len();
                if total > RECENT_MAX {
                    items.drain(..total - RECENT_MAX);
                }
                coordination.insert(key.into(), Value::Array(items));
            }
        }
        if bounds.summary {
            coordination.insert("tasks".into(), Value::Array(Vec::new()));
        }
        value.insert("coordination".into(), Value::Object(coordination));
    }
    if bounds.include_board && !bounds.summary && detail != "overview" {
        value.insert("board".into(), board(&[]));
    }
    value.insert("actors_runtime".into(), Value::Array(Vec::new()));
    if bounds.include_actors {
        value.insert("agent_states".into(), Value::Array(Vec::new()));
    }
    value.insert("meta".into(), Value::Object(document.meta.clone()));
    value
}

fn render_task(task: &Map<String, Value>, full: bool) -> Value {
    if full {
        Value::Object(task.clone())
    } else {
        Value::Object(summary_task(task))
    }
}

fn board(page: &[Value]) -> Value {
    let mut result = Map::new();
    for wanted in ["planned", "active", "done", "archived"] {
        result.insert(
            wanted.into(),
            Value::Array(
                page.iter()
                    .filter(|task| task_status(task) == wanted)
                    .cloned()
                    .collect(),
            ),
        );
    }
    Value::Object(result)
}

fn task_summary(tasks: &[Map<String, Value>]) -> Value {
    let count = |wanted: &str| tasks.iter().filter(|task| status(task) == wanted).count();
    let blocked = tasks
        .iter()
        .filter(|task| !matches!(status(task), "done" | "archived") && is_blocked(task))
        .count();
    let waiting_user = tasks
        .iter()
        .filter(|task| task.get("waiting_on").and_then(Value::as_str) == Some("user"))
        .count();
    let pending_handoffs = tasks
        .iter()
        .filter(|task| {
            task.get("handoff_to")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
        })
        .count();
    json!({
        "total":tasks.iter().filter(|task| status(task) != "archived").count(),
        "planned":count("planned"),
        "active":count("active"),
        "done":count("done"),
        "archived":count("archived"),
        "blocked":blocked,
        "waiting_user":waiting_user,
        "pending_handoffs":pending_handoffs,
    })
}

fn attention(tasks: &[Map<String, Value>]) -> Value {
    let live = || {
        tasks
            .iter()
            .filter(|task| !matches!(status(task), "done" | "archived"))
    };
    json!({
        "blocked":live().filter(|task| is_blocked(task)).count(),
        "waiting_user":live()
            .filter(|task| task.get("waiting_on").and_then(Value::as_str) == Some("user"))
            .count(),
        "pending_handoffs":live()
            .filter(|task| {
                task.get("handoff_to")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            })
            .count(),
    })
}

fn summary_task(task: &Map<String, Value>) -> Map<String, Value> {
    const FIELDS: [&str; 18] = [
        "id",
        "title",
        "outcome",
        "parent_id",
        "status",
        "archived_from",
        "assignee",
        "priority",
        "blocked_by",
        "waiting_on",
        "handoff_to",
        "task_type",
        "notes",
        "checklist",
        "created_at",
        "updated_at",
        "progress",
        "is_root",
    ];
    FIELDS
        .into_iter()
        .filter_map(|field| {
            task.get(field)
                .filter(|value| !value.is_null())
                .cloned()
                .map(|value| (field.into(), value))
        })
        .collect()
}

fn status(task: &Map<String, Value>) -> &str {
    task.get("status")
        .and_then(Value::as_str)
        .unwrap_or("planned")
}

fn is_blocked(task: &Map<String, Value>) -> bool {
    task.get("blocked_by")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty())
        || matches!(
            task.get("waiting_on").and_then(Value::as_str),
            Some("actor" | "external")
        )
}

fn task_status(task: &Value) -> &str {
    task.get("status")
        .and_then(Value::as_str)
        .unwrap_or("planned")
}

fn serialized_len<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "context_bounds_tests.rs"]
mod tests;
