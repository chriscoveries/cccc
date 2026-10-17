//! Tests only: observe the exact real submitted turn, never a stale idle state.
use super::super::codex_voice_analyst::AnalystEvent;
use super::supervisor;
use std::time::Duration;

pub(crate) struct Completion {
    group: String,
    actor: String,
    generation: String,
    events: tokio::sync::broadcast::Receiver<AnalystEvent>,
}

impl Completion {
    pub(crate) fn subscribe(group: &str, actor: &str) -> Self {
        let item = supervisor::lookup(&(group.to_owned(), actor.to_owned()))
            .expect("owned fixture session");
        Self {
            group: group.to_owned(),
            actor: actor.to_owned(),
            generation: item.managed.generation().to_owned(),
            events: item.managed.subscribe(),
        }
    }

    pub(crate) fn wait(mut self, delegation: &str) {
        super::super::block_on_managed(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                let mut selected = None;
                loop {
                    let event = self.events.recv().await.expect("fixture event");
                    if event.generation != self.generation {
                        continue;
                    }
                    if event.message["method"] == "turn/started"
                        && event.requested_delegation_id.as_deref() == Some(delegation)
                    {
                        selected = event
                            .message
                            .pointer("/params/turn/id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned);
                    }
                    if event.message["method"] == "turn/completed"
                        && selected.as_deref().is_some_and(|id| {
                            event
                                .message
                                .pointer("/params/turn/id")
                                .and_then(serde_json::Value::as_str)
                                == Some(id)
                        })
                    {
                        assert_eq!(
                            event
                                .message
                                .pointer("/params/turn/status")
                                .and_then(serde_json::Value::as_str),
                            Some("completed")
                        );
                        break;
                    }
                }
            })
            .await
            .expect("new submitted fixture turn must really complete");
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !supervisor::ready_for_mail_notice(&self.group, &self.actor) {
            assert!(
                std::time::Instant::now() < deadline,
                "fixture idle projection"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
