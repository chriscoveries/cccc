//! A transcript observation is evidence, never authorization to adopt history.
use super::*;

pub(super) fn require_clear(document: &Map<String, Value>) -> io::Result<()> {
    let Some(conflict) = document.get("claude_identity_conflict") else {
        return Ok(());
    };
    let worker = string(document, "provider_session_id");
    let ids: std::collections::BTreeSet<&str> = conflict["observed_transcripts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value["session_id"].as_str())
        .filter(|id| *id != worker && valid_session_id(id))
        .collect();
    let target = if ids.len() == 1 {
        (*ids.first().expect("one validated syntax ID")).to_owned()
    } else {
        "<session-id>".to_owned()
    };
    Err(io::Error::other(ResumeBlocked(format!(
        "Saved Claude worker {worker} reported a different conversation; Resume {target} (adopts that conversation) or New Session."
    ))))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn observe(
    home: &HomeLayout,
    group: &str,
    actor: &str,
    cwd: &Path,
    environment: &BTreeMap<String, String>,
    document: &mut Map<String, Value>,
) -> io::Result<()> {
    require_clear(document)?;
    let worker = string(document, "provider_session_id");
    let observed = super::super::super::codex_voice_analyst::inspect_claude_receipt_identity(
        environment, cwd, &worker,
    ).map_err(|error| io::Error::other(ResumeBlocked(format!(
        "Could not verify saved Claude worker {worker}: {error}; Resume <session-id> (adopts that conversation) or New Session."
    ))))?;
    if let Some(conflict) = observed {
        document.insert("claude_identity_conflict".into(), conflict);
        document.insert("status".into(), json!("identity_conflict"));
        document.insert("resume_eligible".into(), json!(false));
        document.insert("updated_at".into(), json!(utc_now()));
        let error = require_clear(document).expect_err("observed conflict is present");
        document.insert("last_resume_error".into(), json!(error.to_string()));
        write(home, group, actor, document)?;
        return Err(error);
    }
    Ok(())
}
