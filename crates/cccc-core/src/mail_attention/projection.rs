use super::*;
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct Projection {
    pub ids: Vec<String>,
    pub first_at: i64,
    pub opened_by: String,
    pub hash: String,
    pub unread_count: usize,
    pub expired_count: usize,
    pub invalid_count: usize,
    pub expired_through: Option<i64>,
}

pub(super) fn project(
    group: &GroupDoc,
    actor: &Actor,
    events: &[Event],
    positions: &HashMap<String, usize>,
    cursor: Option<&str>,
    now: i64,
    watermark: Option<i64>,
) -> Projection {
    let generation = inbox::actor_generation_positions(events)
        .get(&actor.id)
        .copied()
        .unwrap_or(0);
    let cursor_position = cursor.and_then(|id| positions.get(id)).copied();
    let cursor_unknown = cursor.is_some() && cursor_position.is_none();
    let mut replied = HashSet::new();
    let mut deliveries = BTreeMap::new();
    for event in &events[generation..] {
        if event.kind == "chat.message"
            && event.by == actor.id
            && let Some(id) = event.data.get("reply_to").and_then(Value::as_str)
        {
            replied.insert(id);
        }
        if event.kind == "runtime.delivery"
            && event.data.get("actor_id").and_then(Value::as_str) == Some(&actor.id)
            && let (Some(id), Some(state)) = (
                event.data.get("source_event_id").and_then(Value::as_str),
                event.data.get("state").and_then(Value::as_str),
            )
        {
            deliveries.insert(id, state);
        }
    }
    let mut result = Projection::default();
    for (position, event) in events.iter().enumerate().skip(generation) {
        if cursor_position.is_some_and(|cursor| position <= cursor)
            || !inbox::is_mail_for_actor(group, event, &actor.id)
        {
            continue;
        }
        result.unread_count += 1;
        let Some(at) = DateTime::parse_from_rfc3339(&event.ts)
            .ok()
            .map(|t| t.timestamp())
        else {
            result.invalid_count += 1;
            continue;
        };
        if at > now {
            result.invalid_count += 1;
            continue;
        }
        if now.saturating_sub(at) >= EXPIRY_SECONDS
            || watermark.is_some_and(|expired| at <= expired)
        {
            result.expired_count += 1;
            result.expired_through = Some(result.expired_through.map_or(at, |old| old.max(at)));
            continue;
        }
        let to = event.data.get("to").and_then(Value::as_array);
        let concrete = to.is_some_and(|to| {
            !to.is_empty()
                && !to
                    .iter()
                    .any(|v| matches!(v.as_str(), Some("@all" | "@peers" | "@foreman")))
        });
        if !actor.enabled
            || actor.internal_kind.is_some()
            || cursor_unknown
            || !concrete
            || replied.contains(event.id.as_str())
            || deliveries
                .get(event.id.as_str())
                .is_some_and(|state| matches!(*state, "accepted" | "ambiguous"))
        {
            continue;
        }
        if result.ids.is_empty() {
            result.first_at = at;
            result.opened_by = event.id.clone();
        }
        result.first_at = result.first_at.min(at);
        result.ids.push(event.id.clone());
    }
    result.ids.sort();
    let identity = incarnation(actor);
    result.hash = digest(&[
        &group.group_id,
        &actor.id,
        &identity,
        &POLICY_VERSION.to_string(),
        &result.ids.join("\0"),
    ]);
    result
}
