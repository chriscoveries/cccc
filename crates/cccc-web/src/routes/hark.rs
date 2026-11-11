//! Hark's agent-bound entrance and owner setup surface. No host metadata or push.
use super::{
    web_model_connector_provisioning as provisioning, web_model_connector_session as session,
    web_model_connector_store as store, web_model_delivery_completion::call,
};
use crate::{
    AppState,
    api::{ApiError, ApiResult, success},
};
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
struct ActorQuery {
    group_id: String,
    actor_id: String,
}

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/api/v1/web-model/hark", get(status).post(change))
}

async fn snapshot(s: &AppState, group: &str, actor: &str) -> Result<Value, ApiError> {
    let args = json!({"by":"user","group_id":group,"actor_id":actor});
    call(
        s,
        "hark_pull_status",
        args.as_object().expect("object").clone(),
    )
    .await
}

async fn status(State(s): State<AppState>, Query(q): Query<ActorQuery>) -> ApiResult {
    Ok(success(snapshot(&s, &q.group_id, &q.actor_id).await?))
}

async fn change(State(s): State<AppState>, Json(body): Json<Value>) -> ApiResult {
    let group = super::web_model_connectors::required(&body, "group_id")?;
    let actor = super::web_model_connectors::required(&body, "actor_id")?;
    let action = super::web_model_connectors::required(&body, "action")?;
    let mut args = json!({"by":"user","group_id":group,"actor_id":actor})
        .as_object()
        .expect("object")
        .clone();
    let op = match action.as_str() {
        "create" | "rotate" => {
            args.insert("rotate".into(), json!(action == "rotate"));
            "hark_connector_configure"
        }
        "revoke" => "hark_connector_revoke",
        "resolve" => {
            args.insert(
                "turn_id".into(),
                json!(super::web_model_connectors::required(&body, "turn_id")?),
            );
            args.insert("event_ids".into(), body["event_ids"].clone());
            "hark_work_resolve"
        }
        _ => return Err(ApiError::bad("invalid_hark_action")),
    };
    let result = call(&s, op, args).await?;
    let mut response = snapshot(&s, &group, &actor).await?;
    if matches!(action.as_str(), "create" | "rotate") {
        let secret = result["secret"]
            .as_str()
            .ok_or_else(|| ApiError::bad("hark_credential_unavailable"))?;
        let base = provisioning::connector_base_url(&s)?;
        let mut connector = provisioning::public(&result["connector"], &base);
        connector["connector_url_path_token"] = json!(format!(
            "{}/token/{secret}",
            connector["connector_url"].as_str().unwrap_or_default()
        ));
        connector["secret_available"] = json!(true);
        response["connector"] = connector;
        response["secret"] = json!(secret);
    }
    Ok(success(response))
}

pub(super) fn extend_catalog(response: &mut Value) {
    let Some(tools) = response["result"]["tools"].as_array_mut() else {
        return;
    };
    tools.retain(|t| {
        t["name"]
            .as_str()
            .is_some_and(|n| cccc_core::HARK_TOOL_NAMES.contains(&n))
    });
    for tool in tools.iter_mut() {
        let wait = tool["name"] == "cccc_runtime_wait_next_turn";
        if wait {
            tool["description"] = json!(
                "Hark scheduled poll: call once with {} using the agent-bound connector credential. Returns immediately when idle. A successful claim returns turn and execution actor_token. On turn_in_progress or stopped, exit; never replay unfinished work. Keep turn.turn_id, turn.event_ids and actor_token together until confirmed completion."
            );
            tool["inputSchema"] =
                json!({"type":"object","properties":{},"additionalProperties":false});
        } else {
            tool["inputSchema"]["properties"]["actor_token"] = json!({"type":"string","minLength":43,"maxLength":43,
                "description":format!("Temporary execution credential returned by this run's successful Hark claim. The connector credential cannot authorize business tools. {}",session::CREDENTIAL_ARGUMENT_USAGE)});
            if !tool["inputSchema"]["required"].is_array() {
                tool["inputSchema"]["required"] = json!([]);
            }
            tool["inputSchema"]["required"]
                .as_array_mut()
                .expect("array")
                .push(json!("actor_token"));
        }
    }
    tools.push(json!({"name":"cccc_connector_status","description":"Read this Hark actor's connection and unfinished-turn status. Uses the fixed connector route; does not claim or execute work.",
        "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}}));
}

pub(super) async fn handle(s: &AppState, c: &Value, r: &Value) -> Option<Value> {
    if c["kind"] != "hark_connector" || r["method"] != "tools/call" {
        return None;
    }
    let name = r["params"]["name"].as_str().unwrap_or_default();
    if name != "cccc_connector_status" {
        return None;
    }
    if r["params"]
        .get("arguments")
        .is_some_and(|a| !a.as_object().is_some_and(|a| a.is_empty()))
    {
        return Some(session::result(
            r,
            json!({"error":"invalid_arguments"}),
            true,
        ));
    }
    Some(match store::resolve(s, c, r) {
        Ok(b) => match snapshot(
            s,
            b["group_id"].as_str().unwrap_or_default(),
            b["actor_id"].as_str().unwrap_or_default(),
        )
        .await
        {
            Ok(status) => session::result(r, status, false),
            Err(e) => session::result(r, json!({"error":e.to_string()}), true),
        },
        Err(e) => session::result(r, json!({"error":e.to_string()}), true),
    })
}

pub(super) async fn claim(s: &AppState, r: &Value, b: &Value) -> Value {
    let args = json!({"group_id":b["group_id"],"actor_id":b["actor_id"],"by":b["actor_id"],"_cccc_web_binding":b});
    match call(
        s,
        "hark_work_claim",
        args.as_object().expect("object").clone(),
    )
    .await
    {
        Ok(result) => session::result(r, result, false),
        Err(e) => session::result(r, json!({"error":e.to_string()}), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_has_pull_and_messages_without_workspace_execution() {
        let mut response = json!({"result":{"tools":cccc_mcp::web_model_catalog()}});
        extend_catalog(&mut response);
        let tools = response["result"]["tools"].as_array().expect("catalog");
        for name in [
            "cccc_message_send",
            "cccc_message_reply",
            "cccc_bootstrap",
            "cccc_runtime_complete_turn",
        ] {
            let tool = tools.iter().find(|t| t["name"] == name).expect("tool");
            assert!(
                tool["inputSchema"]["required"]
                    .as_array()
                    .expect("required")
                    .contains(&json!("actor_token"))
            );
        }
        for name in [
            "cccc_file",
            "cccc_shell",
            "cccc_code_exec",
            "cccc_repo",
            "cccc_pair",
            "cccc_capability_install",
        ] {
            assert!(!tools.iter().any(|t| t["name"] == name));
            assert!(!cccc_core::hark_tool_allowed(name));
        }
        let wait = tools
            .iter()
            .find(|t| t["name"] == "cccc_runtime_wait_next_turn")
            .expect("wait");
        assert_eq!(wait["inputSchema"]["properties"], json!({}));
    }
}
