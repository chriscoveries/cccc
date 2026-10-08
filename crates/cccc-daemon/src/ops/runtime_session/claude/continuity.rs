use super::*;

pub(super) fn without_model(command: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < command.len() {
        if command[index] == "--" {
            result.extend_from_slice(&command[index..]);
            break;
        }
        if matches!(command[index].as_str(), "--model" | "-m") {
            index += 2;
        } else if command[index].starts_with("--model=") {
            index += 1;
        } else {
            result.push(command[index].clone());
            index += 1;
        }
    }
    result
}

pub(super) fn prior_command(command: &[String], model: &str) -> Vec<String> {
    if model.is_empty() {
        return without_model(command);
    }
    let mut result = command.to_vec();
    let mut found = false;
    let mut index = 0;
    while index < result.len() {
        if result[index] == "--" {
            break;
        }
        if matches!(result[index].as_str(), "--model" | "-m") {
            if let Some(value) = result.get_mut(index + 1) {
                *value = model.into();
                found = true;
            }
            index += 2;
        } else if result[index].starts_with("--model=") {
            result[index] = format!("--model={model}");
            found = true;
            index += 1;
        } else {
            index += 1;
        }
    }
    if !found {
        let index = result
            .iter()
            .position(|item| item == "--")
            .unwrap_or(result.len());
        result.splice(index..index, ["--model".into(), model.into()]);
    }
    result
}

pub(super) fn previous_sessions(document: &Map<String, Value>) -> Vec<Value> {
    let mut history = document
        .get("previous_sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .take(20)
        .cloned()
        .collect::<Vec<_>>();
    history.reverse();
    history
}

pub(super) fn archive(document: &Map<String, Value>, reason: &str) -> Vec<Value> {
    let mut history = previous_sessions(document);
    let id = string(document, "provider_session_id");
    if valid_session_id(&id) {
        history.push(
            json!({"session_id":id,"workspace_path":string(document,"workspace_path"),
            "model":string(document,"model"),"replaced_at":utc_now(),"reason":reason}),
        );
        if history.len() > 20 {
            history.remove(0);
        }
    }
    history
}

pub(super) fn decision(
    home: &HomeLayout,
    group: &str,
    actor: &str,
    document: &mut Map<String, Value>,
    action: &str,
    reason: &str,
) -> io::Result<()> {
    let timestamp = utc_now();
    document.insert(
        "last_recovery_decision".into(),
        json!({"action":action,"reason":reason,
        "session_id":string(document,"provider_session_id"),"at":timestamp}),
    );
    document.insert("updated_at".into(), json!(timestamp));
    write(home, group, actor, document)?;
    let mut event = cccc_contracts::Event::new("runtime.session.recovery", group);
    event.by = "system".into();
    event.data = Map::from_iter([
        ("actor_id".into(), json!(actor)),
        ("action".into(), json!(action)),
        ("reason".into(), json!(reason)),
        (
            "session_id".into(),
            json!(string(document, "provider_session_id")),
        ),
    ]);
    cccc_core::ledger::append(
        &cccc_core::GroupStore::new(home.clone())?.ledger_path(group)?,
        &event,
    )?;
    Ok(())
}

pub(super) fn blocked(
    home: &HomeLayout,
    group: &str,
    actor: &str,
    document: &mut Map<String, Value>,
    field: &str,
) -> io::Result<Option<ResumeAttempt>> {
    let reason = format!(
        "Saved Claude session {} cannot resume: {field} changed",
        string(document, "provider_session_id")
    );
    document.insert("last_resume_error".into(), json!(reason));
    document.insert("status".into(), json!("resume_failed"));
    document.insert("resume_eligible".into(), json!(false));
    decision(home, group, actor, document, "block", field)?;
    Err(io::Error::other(ResumeBlocked(reason)))
}
