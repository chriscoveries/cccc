//! Bounded projection of the context snapshot.
//!
//! A group with ten thousand cards projects to a multi-megabyte answer; a
//! single `cccc_context_get` then fails the client's transport and the client
//! retries until the session corrupts (owner spec dd17cbba, items 1-2). The
//! daemon therefore bounds every projection by default:
//!
//! * `limit` caps how many board/task items one answer carries (default 100),
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

use cccc_contracts::DaemonRequest;
use cccc_core::GroupDoc;
use cccc_core::context::ContextDoc;
use serde_json::{Map, Value, json};

use crate::dispatch::{OpError, bool_arg, string_arg};

pub(super) const DEFAULT_LIMIT: usize = 100;
pub(super) const MAX_LIMIT: usize = 1_000;
pub(super) const DEFAULT_MAX_BOARD_ITEMS: usize = 100;
pub(super) const DEFAULT_STATE_MAX_BYTES: usize = 256 * 1024;

const CURSOR_PREFIX: &str = "v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Bounds {
    pub limit: usize,
    pub max_board_items: usize,
    pub max_bytes: usize,
    pub include_coordination: bool,
    pub include_board: bool,
    pub include_actors: bool,
    pub summary: bool,
    pub task_offset: usize,
    pub actor_offset: usize,
}

impl Bounds {
    /// `None` means the caller asked for the complete, unbounded snapshot.
    pub(super) fn from_request(
        request: &DaemonRequest,
        group: &GroupDoc,
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
                    return Ok(None);
                }
                usize::try_from(raw).unwrap_or(MAX_LIMIT).min(MAX_LIMIT)
            }
        };
        let max_board_items = config_usize(group, "max_board_items")
            .unwrap_or(DEFAULT_MAX_BOARD_ITEMS)
            .clamp(1, MAX_LIMIT);
        let max_bytes = config_usize(group, "state_max_bytes")
            .unwrap_or(DEFAULT_STATE_MAX_BYTES)
            .max(1024);
        let (include_coordination, include_board, include_actors) = include_set(request)?;
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
            summary: bool_arg(request, "summary", false),
            task_offset,
            actor_offset,
        }))
    }

    fn page_size(&self) -> usize {
        self.limit.min(self.max_board_items)
    }
}

fn include_set(request: &DaemonRequest) -> Result<(bool, bool, bool), OpError> {
    let Some(value) = request.args.get("include") else {
        return Ok((true, true, true));
    };
    if value.is_null() {
        return Ok((true, true, true));
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

pub(super) fn project(
    document: &ContextDoc,
    version: &str,
    detail: &str,
    bounds: &Bounds,
) -> Value {
    let total_tasks = document.tasks.len();
    let actors: Vec<(&String, &Map<String, Value>)> = document.agent_states.iter().collect();
    let total_actors = actors.len();
    let page_size = bounds.page_size();

    // Render the candidate page once, keeping each item's own size so the byte
    // cap can be applied without re-serializing the whole answer per item.
    let mut task_end = bounds.task_offset.min(total_tasks);
    let mut page: Vec<Value> = Vec::new();
    let mut actor_end = bounds.actor_offset.min(total_actors);
    let mut actor_page: Vec<Value> = Vec::new();
    if !bounds.summary {
        let budget = bounds.max_bytes;
        let mut used = serialized_len(&envelope(document, version, detail, bounds));
        if bounds.include_coordination || bounds.include_board {
            while page.len() < page_size && task_end < total_tasks {
                let rendered = render_task(&document.tasks[task_end], detail == "full");
                let size = serialized_len(&rendered) + 1;
                if !page.is_empty() && used.saturating_add(size) > budget {
                    break;
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
                let rendered = Value::Object(state);
                let size = serialized_len(&rendered) + 1;
                if !actor_page.is_empty() && used.saturating_add(size) > budget {
                    break;
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
            document,
            version,
            detail,
            bounds,
            &page,
            &actor_page,
            task_end,
            actor_end,
            total_tasks,
            total_actors,
        );
        if serialized_len(&value) <= bounds.max_bytes {
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
            // it rather than failing; the op never errors on size.
            return value;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn assemble(
    document: &ContextDoc,
    version: &str,
    detail: &str,
    bounds: &Bounds,
    page: &[Value],
    actor_page: &[Value],
    task_end: usize,
    actor_end: usize,
    total_tasks: usize,
    total_actors: usize,
) -> Value {
    let mut value = envelope(document, version, detail, bounds);
    if bounds.include_coordination && !bounds.summary {
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
    if bounds.include_board && !bounds.summary {
        value.insert("board".into(), board(page));
    }
    if bounds.include_actors {
        value.insert("agent_states".into(), Value::Array(actor_page.to_vec()));
    }
    // A section the caller did not ask for is complete by definition: it must
    // not keep the cursor alive, or an actors-only page would never terminate.
    let tasks_requested = bounds.include_coordination || bounds.include_board;
    let tasks_done = bounds.summary || !tasks_requested || task_end >= total_tasks;
    let actors_done = bounds.summary || !bounds.include_actors || actor_end >= total_actors;
    value.insert(
        "truncated".into(),
        Value::Bool(!(tasks_done && actors_done)),
    );
    value.insert(
        "next_cursor".into(),
        if tasks_done && actors_done {
            Value::Null
        } else {
            Value::String(next_cursor(task_end, actor_end))
        },
    );
    value.insert(
        "paging".into(),
        json!({
            "limit": bounds.page_size(),
            "task_offset": bounds.task_offset.min(total_tasks),
            "task_count": page.len(),
            "task_total": total_tasks,
            "actor_offset": bounds.actor_offset.min(total_actors),
            "actor_count": actor_page.len(),
            "actor_total": total_actors,
            "max_bytes": bounds.max_bytes,
        }),
    );
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
    value.insert("tasks_summary".into(), task_summary(&document.tasks));
    value.insert("attention".into(), attention(&document.tasks));
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
        if detail == "full" {
            for key in ["recent_decisions", "recent_handoffs"] {
                coordination.insert(
                    key.into(),
                    document
                        .coordination
                        .get(key)
                        .cloned()
                        .unwrap_or_else(|| json!([])),
                );
            }
        }
        if bounds.summary {
            coordination.insert("tasks".into(), Value::Array(Vec::new()));
        }
        value.insert("coordination".into(), Value::Object(coordination));
    }
    if bounds.include_board && !bounds.summary {
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
