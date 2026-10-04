use super::*;
use std::collections::HashMap;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Cache {
    pub version: u32,
    pub actors: BTreeMap<String, ActorState>,
    pub last_replay_event: Option<String>,
}

pub(super) fn replay(events: &[Event]) -> io::Result<Cache> {
    let mut cache = Cache {
        version: POLICY_VERSION,
        ..Cache::default()
    };
    for event in events.iter().filter(|e| e.kind == "mail.attention") {
        if event.data.get("version").and_then(Value::as_u64) != Some(u64::from(POLICY_VERSION)) {
            return Err(io::Error::other(
                "unsupported mail attention journal version",
            ));
        }
        let action = event
            .data
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| io::Error::other("missing mail attention action"))?;
        if action == "legacy_rule_retired" {
            if event
                .data
                .get("previous_rule")
                .is_none_or(|r| !r.is_object())
            {
                return Err(io::Error::other("invalid mail attention migration fact"));
            }
            continue;
        }
        if !matches!(
            action,
            "episode_opened"
                | "projection_updated"
                | "episode_closed"
                | "expiry_watermark"
                | "legacy_seeded"
                | "token_reserved"
                | "context_offered"
                | "presentation_accepted"
                | "presentation_failed"
                | "presentation_ambiguous"
        ) {
            return Err(io::Error::other("unknown mail attention journal action"));
        }
        let actor_id = event
            .data
            .get("actor_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| io::Error::other("invalid mail attention actor"))?;
        let state: ActorState = serde_json::from_value(
            event
                .data
                .get("state")
                .cloned()
                .ok_or_else(|| io::Error::other("missing mail attention state"))?,
        )
        .map_err(io::Error::other)?;
        if state.incarnation.is_empty()
            || state.standalone_wakeups > MAX_STANDALONE_WAKEUPS
            || (state.episode_id.is_none() && state.pending.is_some())
            || state
                .last_presentation_at
                .is_some_and(|at| state.due_at < at)
            || state.pending.as_ref().is_some_and(|r| {
                r.token.is_empty() || r.owner.is_empty() || r.carrier_ids.is_empty()
            })
        {
            return Err(io::Error::other("invalid mail attention journal state"));
        }
        if let Some(prior) = cache.actors.get(actor_id)
            && prior.incarnation == state.incarnation
            && (prior
                .expiry_watermark
                .is_some_and(|at| state.expiry_watermark.is_none_or(|new| new < at))
                || prior
                    .last_presentation_at
                    .is_some_and(|at| state.last_presentation_at.is_none_or(|new| new < at))
                || (prior.episode_id.is_some()
                    && prior.episode_id == state.episode_id
                    && (state.attempt_no < prior.attempt_no
                        || state.standalone_wakeups < prior.standalone_wakeups)))
        {
            return Err(io::Error::other(
                "mail attention journal regressed its budget/clock",
            ));
        }
        cache.actors.insert(actor_id.to_owned(), state);
    }
    cache.last_replay_event = events.last().map(|e| e.id.clone());
    Ok(cache)
}

pub(super) struct Transaction {
    group: GroupDoc,
    events: Vec<Event>,
    positions: HashMap<String, usize>,
    cursors: BTreeMap<String, String>,
    pub cache: Cache,
    previous_cache: Option<Cache>,
    now: i64,
    owner: String,
    ledger_path: PathBuf,
    cache_path: PathBuf,
    offered_carriers: HashSet<String>,
}

impl Transaction {
    pub fn replay(
        group: GroupDoc,
        events: Vec<Event>,
        cursors: BTreeMap<String, String>,
        now: i64,
        owner: &str,
        ledger_path: PathBuf,
        cache_path: PathBuf,
    ) -> io::Result<Self> {
        // Never trust the cache to reset budgets. A corrupt/missing cache must
        // successfully replay the entire relevant journal before offering input.
        let previous_cache = fs::read_json::<Cache>(&cache_path).ok();
        let mut cache = replay(&events)?;
        cache
            .actors
            .retain(|id, _| actors::find(&group, id).is_some());
        let offered_carriers = events
            .iter()
            .filter(|e| e.kind == "mail.attention")
            .flat_map(|e| {
                let identity = e
                    .data
                    .get("state")
                    .and_then(|s| s.get("incarnation"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                e.data
                    .get("carrier_ids")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(move |id| format!("{identity}:{id}"))
            })
            .collect();
        let positions = events
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        Ok(Self {
            group,
            events,
            positions,
            cursors,
            cache,
            previous_cache,
            now,
            owner: owner.into(),
            ledger_path,
            cache_path,
            offered_carriers,
        })
    }

    fn record(
        &mut self,
        actor_id: &str,
        action: &str,
        state: ActorState,
        extra: Value,
    ) -> io::Result<()> {
        let mut event = Event::new("mail.attention", &self.group.group_id);
        event.ts = DateTime::<Utc>::from_timestamp(self.now, 0)
            .ok_or_else(|| io::Error::other("invalid attention clock"))?
            .to_rfc3339();
        event.by = "system".into();
        event.data = json!({"version":POLICY_VERSION,"action":action,"actor_id":actor_id,"state":state,"detail":extra}).as_object().cloned().expect("object");
        if let Some(pending) = state.pending.as_ref() {
            event
                .data
                .insert("carrier_ids".into(), json!(pending.carrier_ids));
        } else if action == "context_offered" {
            event
                .data
                .insert("carrier_ids".into(), extra["carrier_ids"].clone());
        }
        ledger::append(&self.ledger_path, &event)?;
        let verified = ledger::inspect(&self.ledger_path, |events, _| {
            events.iter().any(|e| e == &event)
        })?;
        if !verified {
            return Err(io::Error::other(
                "attention reservation not durably verified",
            ));
        }
        self.cache.actors.insert(actor_id.into(), state);
        self.cache.last_replay_event = Some(event.id.clone());
        self.positions.insert(event.id.clone(), self.events.len());
        self.events.push(event);
        Ok(())
    }

    pub fn save_cache(&self) -> io::Result<()> {
        if self.cache.actors.is_empty() && self.previous_cache.is_none() {
            return Ok(());
        }
        if self.previous_cache.as_ref() != Some(&self.cache) {
            fs::write_json_committed(&self.cache_path, &self.cache)?;
        }
        Ok(())
    }

    fn projection(&self, actor: &Actor, state: &ActorState) -> Projection {
        project(
            &self.group,
            actor,
            &self.events,
            &self.positions,
            self.cursors.get(&actor.id).map(String::as_str),
            self.now,
            state.expiry_watermark,
        )
    }

    pub fn refresh(&mut self) -> io::Result<()> {
        let actors = actors::visible(&self.group).cloned().collect::<Vec<_>>();
        for actor in actors {
            if !actor.enabled {
                continue;
            }
            let actor_id = actor.id.clone();
            let identity = incarnation(&actor);
            let previous = self
                .cache
                .actors
                .get(&actor_id)
                .filter(|s| s.incarnation == identity)
                .cloned();
            let mut state = previous.clone().unwrap_or_else(|| ActorState {
                incarnation: identity,
                ..ActorState::default()
            });
            let closed_between =
                state.episode_id.is_some() && self.closed_between_facts(&actor, &state);
            if let Some(reservation) = state.pending.clone() {
                let outcome = self.events.iter().rev().find_map(|event| {
                    (event.kind == "runtime.delivery"
                        && event.data.get("actor_id").and_then(Value::as_str) == Some(&actor_id)
                        && event
                            .data
                            .get("source_event_id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| reservation.carrier_ids.iter().any(|c| c == id)))
                    .then(|| event.data.get("state").and_then(Value::as_str))
                    .flatten()
                    .filter(|s| matches!(*s, "accepted" | "failed" | "ambiguous"))
                });
                if let Some(outcome) = outcome
                    .map(str::to_owned)
                    .or_else(|| (reservation.owner != self.owner).then(|| "ambiguous".to_owned()))
                {
                    self.finish(&actor_id, &reservation.carrier_ids[0], &outcome)?;
                    state = self
                        .cache
                        .actors
                        .get(&actor_id)
                        .cloned()
                        .expect("finished state");
                }
            }
            // A fully consumed/resolved interval closes the old episode even
            // when fresh mail arrives before the next scan. Partial reads do not.
            if closed_between {
                state.episode_id = None;
                state.opened_by = None;
                state.attention_count = 0;
                state.pending = None;
                self.record(
                    &actor_id,
                    "episode_closed",
                    state.clone(),
                    json!({"cause":"resolved_between_scans"}),
                )?;
            }
            let p = self.projection(&actor, &state);
            if let Some(expired) = p.expired_through
                && state.expiry_watermark.is_none_or(|old| expired > old)
            {
                state.expiry_watermark = Some(expired);
                self.record(
                    &actor_id,
                    "expiry_watermark",
                    state.clone(),
                    json!({"expired_through":expired}),
                )?;
            }
            if p.ids.is_empty() {
                if state.episode_id.is_some() {
                    state.episode_id = None;
                    state.opened_by = None;
                    state.attention_count = 0;
                    state.set_hash = p.hash;
                    state.pending = None;
                    self.record(&actor_id, "episode_closed", state, json!({"cause":if p.expired_count > 0 {"expiry"} else if !actor.enabled {"disabled"} else {"read_reply_or_promotion"}}))?;
                }
                continue;
            }
            if state.episode_id.is_none() {
                state.episode_id = Some(digest(&[
                    &self.group.group_id,
                    &actor_id,
                    &state.incarnation,
                    &p.opened_by,
                ]));
                state.opened_by = Some(p.opened_by.clone());
                state.attempt_no = 0;
                state.standalone_wakeups = 0;
                state.legacy_unverified = false;
                state.due_at = p.first_at.saturating_add(initial_delay(&self.group)).max(
                    state
                        .last_presentation_at
                        .map_or(i64::MIN, |at| at.saturating_add(FIRST_ATTENTION_SECONDS)),
                );
                state.attention_count = p.ids.len();
                state.set_hash = p.hash.clone();
                if previous.is_none() {
                    self.seed_legacy(&actor, &p, &mut state);
                }
                self.record(
                    &actor_id,
                    "episode_opened",
                    state.clone(),
                    json!({"opened_by":p.opened_by}),
                )?;
            } else if state.set_hash != p.hash || state.attention_count != p.ids.len() {
                state.set_hash = p.hash;
                state.attention_count = p.ids.len();
                self.record(
                    &actor_id,
                    "projection_updated",
                    state,
                    json!({"cause":"eligible_set_changed"}),
                )?;
            }
        }
        Ok(())
    }

    fn closed_between_facts(&self, actor: &Actor, state: &ActorState) -> bool {
        let last = self
            .events
            .iter()
            .rposition(|e| {
                e.kind == "mail.attention"
                    && e.data.get("actor_id").and_then(Value::as_str) == Some(&actor.id)
            })
            .unwrap_or(self.events.len());
        for (i, event) in self.events.iter().enumerate().skip(last.saturating_add(1)) {
            let resolution = (event.kind == "mail.read"
                && event.data.get("actor_id").and_then(Value::as_str) == Some(&actor.id))
                || (event.kind == "chat.message"
                    && event.by == actor.id
                    && event.data.contains_key("reply_to"))
                || (event.kind == "runtime.delivery"
                    && event.data.get("actor_id").and_then(Value::as_str) == Some(&actor.id)
                    && matches!(
                        event.data.get("state").and_then(Value::as_str),
                        Some("accepted" | "ambiguous")
                    ));
            if !resolution {
                continue;
            }
            let prefix = &self.events[..=i];
            let cursor = prefix.iter().rev().find_map(|e| {
                (e.kind == "mail.read"
                    && e.data.get("actor_id").and_then(Value::as_str) == Some(&actor.id))
                .then(|| e.data.get("event_id").and_then(Value::as_str))
                .flatten()
            });
            let at = DateTime::parse_from_rfc3339(&event.ts)
                .map_or(self.now, |t| t.timestamp())
                .min(self.now);
            if project(
                &self.group,
                actor,
                prefix,
                &self.positions,
                cursor,
                at,
                state.expiry_watermark,
            )
            .ids
            .is_empty()
            {
                return true;
            }
        }
        false
    }

    fn seed_legacy(&self, actor: &Actor, projection: &Projection, state: &mut ActorState) {
        let generation = inbox::actor_generation_positions(&self.events)
            .get(&actor.id)
            .copied()
            .unwrap_or(0);
        for event in self.events.iter().skip(generation).filter(|e| {
            e.kind == "system.notify"
                && e.data.get("kind").and_then(Value::as_str) == Some("mail_notice")
                && e.data.get("target_actor_id").and_then(Value::as_str) == Some(&actor.id)
        }) {
            let outcome = self.events.iter().rev().find(|e| {
                e.kind == "runtime.delivery"
                    && e.data.get("actor_id").and_then(Value::as_str) == Some(&actor.id)
                    && e.data.get("source_event_id").and_then(Value::as_str) == Some(&event.id)
            });
            let relevant = event
                .data
                .get("context")
                .and_then(|c| c.get("source_event_ids"))
                .and_then(Value::as_array)
                .is_some_and(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .any(|id| projection.ids.iter().any(|p| p == id))
                });
            match outcome
                .and_then(|e| e.data.get("state"))
                .and_then(Value::as_str)
            {
                Some("accepted" | "ambiguous") => {
                    let at = outcome
                        .and_then(|e| DateTime::parse_from_rfc3339(&e.ts).ok())
                        .map(|t| t.timestamp());
                    if let Some(at) = at {
                        state.last_presentation_at =
                            Some(state.last_presentation_at.map_or(at, |old| old.max(at)));
                        if relevant {
                            state.standalone_wakeups = state
                                .standalone_wakeups
                                .saturating_add(1)
                                .min(MAX_STANDALONE_WAKEUPS);
                            state.attempt_no = state.attempt_no.saturating_add(1);
                            state.due_at = state
                                .due_at
                                .max(at.saturating_add(backoff(&actor.id, state.attempt_no)));
                        } else {
                            state.due_at =
                                state.due_at.max(at.saturating_add(FIRST_ATTENTION_SECONDS));
                        }
                    } else {
                        state.legacy_unverified = true;
                    }
                }
                Some("failed") => {}
                _ if relevant => {
                    state.legacy_unverified = true;
                }
                _ => {}
            }
        }
    }

    pub fn present(
        &mut self,
        actor_id: &str,
        carrier_ids: &[String],
        standalone: bool,
        context_offered: bool,
    ) -> io::Result<Option<Hint>> {
        if matches!(self.group.state, GroupState::Paused | GroupState::Stopped)
            || carrier_ids.is_empty()
        {
            return Ok(None);
        }
        let Some(actor) = actors::find(&self.group, actor_id) else {
            return Ok(None);
        };
        if !actor.enabled
            || actor.internal_kind.is_some()
            || carrier_ids.iter().any(|id| {
                id.is_empty()
                    || self
                        .offered_carriers
                        .contains(&format!("{}:{id}", incarnation(actor)))
            })
        {
            return Ok(None);
        }
        let Some(mut state) = self
            .cache
            .actors
            .get(actor_id)
            .filter(|s| s.incarnation == incarnation(actor))
            .cloned()
        else {
            return Ok(None);
        };
        if state.episode_id.is_none()
            || state.attention_count == 0
            || state.pending.is_some()
            || self.now < state.due_at
            || (standalone
                && (!standalone_enabled(&self.group)
                    || state.legacy_unverified
                    || state.standalone_wakeups >= MAX_STANDALONE_WAKEUPS))
        {
            return Ok(None);
        }
        let number = state.attempt_no.saturating_add(1);
        let token = format!("{}:{number}", state.episode_id.as_deref().expect("episode"));
        state.attempt_no = number;
        state.last_presentation_at = Some(self.now);
        state.due_at = self.now.saturating_add(backoff(actor_id, number));
        if !context_offered {
            state.pending = Some(Reservation {
                token: token.clone(),
                owner: self.owner.clone(),
                carrier_ids: carrier_ids.to_vec(),
                standalone,
                reserved_at: self.now,
            });
        }
        let count = state.attention_count;
        let identity = state.incarnation.clone();
        self.record(actor_id, if context_offered {"context_offered"} else {"token_reserved"}, state,
            json!({"token":token,"carrier_ids":carrier_ids,"admission":if standalone {"structured_pull"} else if context_offered {"mcp_context"} else {"ordinary_delivery"}}))?;
        self.offered_carriers
            .extend(carrier_ids.iter().map(|id| format!("{identity}:{id}")));
        Ok(Some(Hint {
            count,
            attention_count: count,
            token,
            action: "cccc_inbox_read()".into(),
        }))
    }

    pub fn validate(&self, actor_id: &str, token: &str) -> io::Result<Option<usize>> {
        if matches!(self.group.state, GroupState::Paused | GroupState::Stopped) {
            return Ok(None);
        }
        let Some(actor) = actors::find(&self.group, actor_id).filter(|a| a.enabled) else {
            return Ok(None);
        };
        let Some(state) = self
            .cache
            .actors
            .get(actor_id)
            .filter(|s| s.incarnation == incarnation(actor))
        else {
            return Ok(None);
        };
        Ok(state
            .pending
            .as_ref()
            .filter(|r| r.token == token && r.owner == self.owner)
            .and_then(|_| (state.attention_count > 0).then_some(state.attention_count)))
    }

    pub fn finish(&mut self, actor_id: &str, source_id: &str, outcome: &str) -> io::Result<()> {
        let Some(mut state) = self.cache.actors.get(actor_id).cloned() else {
            return Ok(());
        };
        let Some(pending) = state
            .pending
            .clone()
            .filter(|r| r.carrier_ids.iter().any(|id| id == source_id))
        else {
            return Ok(());
        };
        state.pending = None;
        if pending.standalone && matches!(outcome, "accepted" | "ambiguous") {
            state.standalone_wakeups = state
                .standalone_wakeups
                .saturating_add(1)
                .min(MAX_STANDALONE_WAKEUPS);
        }
        self.record(actor_id, &format!("presentation_{outcome}"), state,
            json!({"token":pending.token,"carrier_ids":pending.carrier_ids,"budget_exhausted":pending.standalone && matches!(outcome,"accepted"|"ambiguous")
                && self.cache.actors.get(actor_id).is_some_and(|s| s.standalone_wakeups.saturating_add(1) >= MAX_STANDALONE_WAKEUPS)}))
    }
}
