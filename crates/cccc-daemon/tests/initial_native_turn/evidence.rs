//! Offline historical-capture verifier; never imported by the daemon.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct Fixture {
    pub schema: u32,
    pub provider: String,
    pub source_sha: String,
    pub binary_sha256: String,
    pub provider_fixture_sha256: String,
    pub cases: Vec<Capture>,
}

#[derive(Deserialize)]
pub struct Capture {
    pub name: String,
    pub runtime: String,
    pub runtime_version: String,
    pub runtime_sha256: String,
    pub source: Source,
    pub observation_end: DateTime<Utc>,
    pub deliveries: Vec<Delivery>,
    pub native: Native,
    pub authenticated_requests: usize,
}

#[derive(Deserialize)]
pub struct Source {
    pub event_id: String,
    pub at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct Delivery {
    pub id: String,
    pub at: DateTime<Utc>,
    pub source_event_id: String,
    pub state: String,
}

#[derive(Deserialize)]
pub struct Native {
    pub snapshot_sha256: String,
    pub session_id: String,
    pub users: Vec<Message>,
    pub assistants: Vec<Assistant>,
}

#[derive(Deserialize)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub created_ms: i64,
    pub parts: Vec<Part>,
}

#[derive(Deserialize)]
pub struct Assistant {
    #[serde(flatten)]
    pub message: Message,
    pub parent_id: String,
    pub completed_ms: Option<i64>,
    pub finish: Option<String>,
    pub error_present: bool,
}

#[derive(Deserialize)]
pub struct Part {
    pub id: String,
    pub session_id: String,
    pub kind: String,
    pub text_bytes: usize,
    pub text_sha256: Option<String>,
    pub source_event_ids: Vec<String>,
    pub startup_headers: usize,
    pub tool_status: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Report<'a> {
    pub source_event_id: &'a str,
    pub transport: &'a str,
    pub delivery_event_ids: Vec<&'a str>,
    pub native_user_id: Option<&'a str>,
    pub native_part_id: Option<&'a str>,
    pub observed_admission: bool,
    pub execution: bool,
    pub backend_outcome: &'static str,
    pub observed_liveness: &'static str,
    pub prepared_payload_match: &'static str,
    pub production_admission: &'static str,
    pub full_proof: &'static str,
}

fn sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit())
}

impl Capture {
    pub fn verify<'a>(&'a self, native: &'a Native) -> Report<'a> {
        let deliveries: Vec<_> = self
            .deliveries
            .iter()
            .filter(|e| {
                e.source_event_id == self.source.event_id
                    && e.at >= self.source.at
                    && e.at <= self.observation_end
            })
            .collect();
        let accepted = deliveries.iter().filter(|e| e.state == "accepted").count();
        let transport = if accepted > 1 {
            "ambiguous"
        } else {
            deliveries.last().map_or("unobserved", |e| e.state.as_str())
        };
        let owned = |m: &Message| {
            m.session_id == native.session_id
                && !m.id.is_empty()
                && m.created_ms >= self.source.at.timestamp_millis()
                && m.created_ms <= self.observation_end.timestamp_millis()
                && m.parts
                    .iter()
                    .all(|p| p.session_id == native.session_id && !p.id.is_empty())
        };
        let matches: Vec<_> = native
            .users
            .iter()
            .filter(|u| owned(u))
            .flat_map(|u| {
                u.parts
                    .iter()
                    .filter(move |p| {
                        p.kind == "text"
                            && p.startup_headers == 1
                            && p.source_event_ids.as_slice() == [self.source.event_id.as_str()]
                            && p.text_bytes > 0
                            && p.text_sha256.as_deref().is_some_and(sha256)
                    })
                    .map(move |p| (u, p))
            })
            .collect();
        let matched = (matches.len() == 1).then(|| matches[0]);
        let consumed: Vec<_> = native
            .assistants
            .iter()
            .filter(|a| {
                owned(&a.message)
                    && matched.is_some_and(|(u, _)| {
                        // OpenCode consumes the ordered queued users through parentID.
                        let first = native.users.iter().position(|m| m.id == u.id);
                        let last = native.users.iter().position(|m| m.id == a.parent_id);
                        first.zip(last).is_some_and(|(first, last)| {
                            first <= last
                                && owned(&native.users[last])
                                && native.users[last].created_ms <= a.message.created_ms
                        })
                    })
            })
            .collect();
        let execution = consumed.iter().any(|a| {
            a.message.parts.iter().any(|p| {
                (p.kind == "tool" && p.tool_status.as_deref() == Some("completed"))
                    || (p.kind == "text"
                        && p.text_bytes > 0
                        && p.text_sha256.as_deref().is_some_and(sha256))
            })
        });
        let latest = consumed.iter().max_by_key(|a| a.message.created_ms);
        let backend_outcome = match latest {
            Some(a) if a.error_present => "failed",
            Some(a) if a.finish.as_deref() == Some("cancelled") => "cancelled",
            Some(a)
                if a.finish.as_deref() == Some("stop")
                    && a.completed_ms.is_some_and(|t| {
                        t >= a.message.created_ms && t <= self.observation_end.timestamp_millis()
                    }) =>
            {
                "completed"
            }
            Some(_) => "unsettled",
            None => "unobserved",
        };
        let observed_liveness = if transport != "accepted"
            || accepted != 1
            || !sha256(&native.snapshot_sha256)
            || self.observation_end <= self.source.at
        {
            "INCONCLUSIVE"
        } else if matched.is_none() {
            "fixture-completed"
        } else if execution && backend_outcome == "completed" {
            "fixture-completed"
        } else {
            "pending-or-unsuccessful"
        };
        Report {
            source_event_id: &self.source.event_id,
            transport,
            delivery_event_ids: deliveries.iter().map(|e| e.id.as_str()).collect(),
            native_user_id: matched.map(|(u, _)| u.id.as_str()),
            native_part_id: matched.map(|(_, p)| p.id.as_str()),
            observed_admission: matched.is_some(),
            execution,
            backend_outcome,
            observed_liveness,
            // This format has no independent pre-write digest, observer generation,
            // durable admission ticket, or real-provider evidence. Never infer them.
            prepared_payload_match: "unknown",
            production_admission: "unknown",
            full_proof: "INCONCLUSIVE",
        }
    }
}
