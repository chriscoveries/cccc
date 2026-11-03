use cccc_contracts::Event;
use serde_json::{Map, Value, json};

use crate::automation::STANDUP_SNIPPET;
use crate::{GroupDoc, actors, inbox};

pub fn notify_events(
    group: &GroupDoc,
    rule_id: &str,
    rule: &Map<String, Value>,
    action: Option<&Map<String, Value>>,
    scheduled_at: &str,
) -> Vec<Event> {
    let mail = match action.and_then(|action| action.get("delivery")) {
        None => false,
        Some(value) if value.as_str() == Some("notification") => false,
        Some(value) if value.as_str() == Some("mail") => true,
        Some(_) => return Vec::new(),
    };
    let template = snippet(group, action).or_else(|| {
        action
            .and_then(|action| action.get("message"))
            .and_then(Value::as_str)
            .filter(|message| !message.trim().is_empty())
            .map(str::to_owned)
    });
    let Some(template) = template else {
        return Vec::new();
    };
    let interval_seconds = rule
        .get("trigger")
        .and_then(|trigger| trigger.get("every_seconds"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let actor_names = actors::visible(group)
        .filter(|actor| actor.enabled && actor.id != "user")
        .map(|actor| {
            if actor.title.trim().is_empty() {
                actor.id.as_str()
            } else {
                actor.title.as_str()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let message = render(
        &template,
        &[
            (
                "interval_minutes",
                if interval_seconds >= 60 {
                    (interval_seconds / 60).to_string()
                } else {
                    "0".into()
                },
            ),
            ("group_title", group.title.clone()),
            ("actor_names", actor_names),
            ("scheduled_at", scheduled_at.to_owned()),
        ],
    )
    .trim()
    .to_owned();
    if message.is_empty() {
        return Vec::new();
    }
    let to = rule
        .get("to")
        .cloned()
        .unwrap_or_else(|| json!(["@foreman"]));
    let mut routing = Event::new("system.notify", &group.group_id);
    routing.by = "system".into();
    routing.data = json!({"to":to}).as_object().cloned().unwrap_or_default();
    actors::visible(group)
        .filter(|actor| {
            actor.enabled && (!mail || actor.id != "user")
                && inbox::is_for_actor(group, &routing, &actor.id)
        })
        .map(|actor| {
            if mail {
                let mut event = Event::new("chat.message", &group.group_id);
                event.by = "system".into();
                event.data = json!({
                    "kind": "automation",
                    "text": message,
                    "to": [actor.id.as_str()],
                    "message_mode": "mail",
                    "context": {"rule_id": rule_id},
                })
                .as_object()
                .cloned()
                .unwrap_or_default();
                return event;
            }
            let mut event = Event::new("system.notify", &group.group_id);
            event.by = "system".into();
            event.data = json!({
                "kind": "automation",
                "title": action.and_then(|action| action.get("title")).and_then(Value::as_str)
                    .filter(|title| !title.trim().is_empty()).unwrap_or("Reminder"),
                "message": message,
                "target_actor_id": actor.id.as_str(),
                "to": [actor.id.as_str()],
                "im_visibility": "public",
                "priority": action.and_then(|action| action.get("priority")).cloned().unwrap_or_else(|| json!("normal")),
                "context": {"rule_id": rule_id},
            })
            .as_object()
            .cloned()
            .unwrap_or_default();
            event
        })
        .collect()
}

fn snippet(group: &GroupDoc, action: Option<&Map<String, Value>>) -> Option<String> {
    let reference = action
        .and_then(|action| action.get("snippet_ref"))
        .and_then(Value::as_str)?;
    let custom = group
        .automation
        .get("snippets")
        .and_then(|value| value.get(reference));
    let overrides = group
        .automation
        .get("snippet_overrides")
        .and_then(|value| value.get(reference));
    if reference == "standup" {
        return overrides
            .or(custom)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| Some(STANDUP_SNIPPET.into()));
    }
    custom
        .or(overrides)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn render(template: &str, context: &[(&str, String)]) -> String {
    let mut output = String::with_capacity(template.len());
    let mut remaining = template;
    while let Some(start) = remaining.find("{{") {
        output.push_str(&remaining[..start]);
        let after_open = &remaining[start + 2..];
        let Some(end) = after_open.find("}}") else {
            output.push_str(&remaining[start..]);
            return output;
        };
        let raw_key = &after_open[..end];
        let key = raw_key.trim();
        if !key.is_empty()
            && key
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            if let Some((_, value)) = context.iter().find(|(name, _)| *name == key) {
                output.push_str(value);
            }
        } else {
            output.push_str("{{");
            output.push_str(raw_key);
            output.push_str("}}");
        }
        remaining = &after_open[end + 2..];
    }
    output.push_str(remaining);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GroupStore, HomeLayout};
    use cccc_contracts::Actor;

    #[test]
    fn delivery_defaults_to_notification_and_mail_excludes_disabled_and_user() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home).expect("store");
        let mut group = store.create("reminder", "").expect("group");
        let mut disabled = Actor::new("disabled");
        disabled.enabled = false;
        group.actors.push(Actor::new("lead"));
        group.actors.push(disabled);
        group.actors.push(Actor::new("user"));
        let rule = json!({"to":["@all"],"trigger":{"every_seconds":60}});
        let mut action = json!({"kind":"notify","message":"Read WORKFLOW.md"});
        let render = |action: &Value| {
            notify_events(
                &group,
                "goal",
                rule.as_object().expect("rule"),
                action.as_object(),
                "now",
            )
        };
        let notifications = render(&action);
        assert!(
            notifications
                .iter()
                .all(|event| event.kind == "system.notify")
        );
        action["delivery"] = json!("mail");
        let mail = render(&action);
        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].kind, "chat.message");
        assert_eq!(mail[0].data["to"], json!(["lead"]));
        assert_eq!(mail[0].data["message_mode"], "mail");
        action["delivery"] = json!("invalid");
        assert!(render(&action).is_empty());
    }
}
