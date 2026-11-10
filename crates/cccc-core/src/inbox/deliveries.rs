//! Payload-free runtime delivery facts. These share the inbox snapshot cache,
//! but keep delivery and legacy chat.read rules separate from notice policy.
//! Like NoticeState, this metadata is O(history); Event bodies are not retained.
use super::*;
use crate::ledger::snapshot::Location;
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct DeliveryState {
    positions: HashMap<String, usize>,
    references: HashSet<String>,
    next_position: usize,
    sources: Vec<Source>,
    actors: HashMap<String, usize>,
    generations: HashMap<String, usize>,
    reads: HashMap<String, Vec<(usize, String)>>,
    latest: HashMap<String, HashMap<String, State>>,
}
struct Source {
    id: String,
    ts: String,
    position: usize,
    location: Location,
    recipients: Vec<usize>,
    mail: bool,
    notification: bool,
    related: Vec<String>,
}
struct State {
    state: String,
    transport: String,
    // pending_sources skips malformed state records; latest_state does not.
    pending: Option<(String, usize, bool)>,
}

impl DeliveryState {
    fn reference_ids(event: &Event) -> Vec<String> {
        match event.kind.as_str() {
            "chat.read" => event
                .data
                .get("event_id")
                .and_then(Value::as_str)
                .map(|id| vec![id.to_owned()])
                .unwrap_or_default(),
            "system.notify" => [
                event.data.get("event_id"),
                event.data.get("related_event_id"),
                event.data.get("context").and_then(|v| v.get("event_id")),
            ]
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
            _ => Vec::new(),
        }
    }
    pub fn remember_references(&mut self, event: &Event) {
        self.references.extend(Self::reference_ids(event));
    }
    pub fn can_append(&self, event: &Event) -> bool {
        Self::reference_ids(event)
            .iter()
            .all(|id| self.positions.contains_key(id) || self.references.contains(id))
    }
    pub fn note(&mut self, group: &GroupDoc, event: &Event, location: Location) {
        let position = self.next_position;
        self.next_position += 1;
        let mode = event.data.get("message_mode").and_then(Value::as_str);
        let notification = event.kind == "system.notify";
        let source = notification
            || (event.kind == "chat.message"
                && matches!(mode, Some("mail" | "send" | "request_reply")));
        if self.references.contains(&event.id) {
            self.positions.entry(event.id.clone()).or_insert(position);
        }
        if let Some(actor) = actor_add_id(event) {
            self.generations.insert(actor.to_owned(), position);
        }
        if source {
            if self.actors.is_empty() {
                self.actors.extend(
                    group
                        .actors
                        .iter()
                        .enumerate()
                        .map(|(index, actor)| (actor.id.clone(), index)),
                );
            }
            let targeted_notice = notification
                && matches!(
                    event.data.get("kind").and_then(Value::as_str),
                    Some("mail_notice" | "reply_notice")
                );
            let recipients = group
                .actors
                .iter()
                .enumerate()
                .filter(|(_, actor)| {
                    event.by != actor.id
                        && if targeted_notice {
                            event.data.get("target_actor_id").and_then(Value::as_str)
                                == Some(actor.id.as_str())
                        } else {
                            is_for_actor(group, event, &actor.id)
                        }
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            if !recipients.is_empty() {
                self.sources.push(Source {
                    id: event.id.clone(),
                    ts: event.ts.clone(),
                    position,
                    location,
                    recipients,
                    mail: !notification && mode == Some("mail"),
                    notification,
                    related: if notification {
                        Self::reference_ids(event)
                    } else {
                        Vec::new()
                    },
                });
            }
        }
        if event.kind == "chat.read" {
            if let (Some(actor), Some(target)) = (
                event.data.get("actor_id").and_then(Value::as_str),
                event.data.get("event_id").and_then(Value::as_str),
            ) {
                self.reads
                    .entry(actor.to_owned())
                    .or_default()
                    .push((position, target.to_owned()));
            }
        } else if event.kind == "runtime.delivery"
            && let (Some(actor), Some(source)) = (
                event.data.get("actor_id").and_then(Value::as_str),
                event.data.get("source_event_id").and_then(Value::as_str),
            )
        {
            // latest_state/claim historically include pre-readd delivery facts.
            // pending() applies the actor generation boundary separately.
            let states = self.latest.entry(actor.to_owned()).or_default();
            let pending = event
                .data
                .get("state")
                .and_then(Value::as_str)
                .map(|state| {
                    (
                        state.to_owned(),
                        position,
                        event.data.get("retryable") == Some(&Value::Bool(true)),
                    )
                })
                .or_else(|| states.get(source).and_then(|state| state.pending.clone()));
            states.insert(
                source.to_owned(),
                State {
                    state: event
                        .data
                        .get("state")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    transport: event
                        .data
                        .get("transport")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    pending,
                },
            );
        }
    }
    pub fn latest(&self, actor: &str, source: &str) -> Option<(String, String)> {
        self.latest
            .get(actor)
            .and_then(|states| states.get(source))
            .map(|state| (state.state.clone(), state.transport.clone()))
    }
    fn pending_sources<'a>(
        &'a self,
        group: &GroupDoc,
        actor: &str,
    ) -> impl Iterator<Item = &'a Source> {
        let actor_index = self.actors.get(actor).copied();
        let external_session =
            find(group, actor).is_some_and(|actor| !actor.external_session_id.is_empty());
        let states = self.latest.get(actor);
        let generation = self.generations.get(actor).copied();
        let current =
            move |position: usize| generation.is_none_or(|generation| position > generation);
        let watermark = self
            .reads
            .get(actor)
            .into_iter()
            .flatten()
            .filter(|(position, _)| current(*position))
            .filter_map(|(read, target)| {
                self.positions
                    .get(target)
                    .copied()
                    .filter(|target| current(*target) && target <= read)
            })
            .max();
        self.sources
            .iter()
            .filter(move |source| {
                current(source.position)
                    && actor_index.is_some_and(|index| source.recipients.contains(&index))
            })
            .filter(move |source| {
                let (state, retryable) = states
                    .and_then(|states| states.get(&source.id))
                    .and_then(|state| state.pending.as_ref())
                    .filter(|(_, position, _)| current(*position))
                    .map(|(state, _, retryable)| (state.as_str(), state == "failed" && *retryable))
                    .unwrap_or(("", false));
                if matches!(state, "accepted" | "ambiguous" | "withdrawn")
                    || (external_session && state == "failed" && !retryable)
                    || (source.mail && state != "claimed" && !retryable)
                {
                    return false;
                }
                if source.notification
                    && let Some(watermark) = watermark
                    && (source.position <= watermark
                        || source.related.iter().any(|id| {
                            self.positions.get(id).is_some_and(|position| {
                                current(*position) && *position <= watermark
                            })
                        }))
                {
                    return false;
                }
                true
            })
    }
    pub fn pending(&self, group: &GroupDoc, actor: &str, limit: usize) -> Vec<Location> {
        self.pending_sources(group, actor)
            .take(limit.max(1))
            .map(|source| source.location)
            .collect()
    }
    pub fn pending_summary(&self, group: &GroupDoc, actor: &str) -> PendingDeliverySummary {
        let now = Utc::now();
        let mut count = 0;
        let mut oldest = None;
        let mut unknown_age = false;
        for source in self.pending_sources(group, actor) {
            count += 1;
            match DateTime::parse_from_rfc3339(&source.ts) {
                Ok(created) if created <= now => {
                    let age = now.signed_duration_since(created).num_seconds() as u64;
                    oldest = Some(oldest.map_or(age, |previous: u64| previous.max(age)));
                }
                _ => unknown_age = true,
            }
        }
        PendingDeliverySummary {
            count,
            oldest_age_seconds: if unknown_age { None } else { oldest },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_contracts::Actor;

    #[test]
    fn delivery_summary_is_uncapped_and_matches_pending_filters() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home).expect("store");
        let mut group = store.create("delivery summary", "").expect("group");
        group.actors.push(Actor::new("hark"));
        let mut state = DeliveryState::default();
        for index in 0..10_005 {
            let mut event = Event::new("chat.message", &group.group_id);
            event.id = format!("source-{index}");
            event.by = "user".into();
            event.ts = "2020-01-01T00:00:00Z".into();
            event.data = json!({"to":["hark"],"message_mode":"send"})
                .as_object()
                .expect("data")
                .clone();
            state.note(
                &group,
                &event,
                Location {
                    source: 0,
                    offset: index as u64,
                    end: index as u64 + 1,
                },
            );
        }
        for (index, outcome) in ["accepted", "ambiguous", "withdrawn", "claimed", "failed"]
            .iter()
            .enumerate()
        {
            let mut event = Event::new("runtime.delivery", &group.group_id);
            event.data = json!({"actor_id":"hark","source_event_id":format!("source-{index}"),"state":outcome}).as_object().expect("data").clone();
            state.note(
                &group,
                &event,
                Location {
                    source: 0,
                    offset: 20_000,
                    end: 20_001,
                },
            );
        }
        let summary = state.pending_summary(&group, "hark");
        assert_eq!(summary.count, 10_002);
        assert_eq!(
            summary.count,
            state.pending(&group, "hark", usize::MAX).len()
        );
        assert!(summary.oldest_age_seconds.expect("age") > 86_400);
        assert_eq!(state.pending_summary(&group, "absent").count, 0);
        state.sources[10].ts = "invalid".into();
        assert_eq!(
            state.pending_summary(&group, "hark").oldest_age_seconds,
            None
        );
    }
}
