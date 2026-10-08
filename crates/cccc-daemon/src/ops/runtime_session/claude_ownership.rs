//! Durable ownership for opt-in Claude detach, separate from continuity receipts.
use super::{path, read, valid_session_id, write};
use cccc_contracts::Actor;
use cccc_core::{GroupStore, HomeLayout};
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Ownership {
    pub group_id: String,
    pub actor_id: String,
    pub actor_created_at: String,
    pub session_id: String,
    pub config_dir: PathBuf,
    pub workspace: PathBuf,
    #[serde(default)]
    pub uncertain: bool,
}

pub(crate) fn saved_session(
    home: &HomeLayout,
    group: &str,
    actor: &str,
) -> io::Result<Option<String>> {
    let doc = match read(home, group, actor) {
        Ok(doc) => doc,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let id = super::string(&doc, "provider_session_id");
    Ok((doc.get("v").and_then(serde_json::Value::as_u64) == Some(2)
        && super::string(&doc, "kind") == "runtime_session"
        && super::string(&doc, "transport") == "claude_agent_view"
        && super::string(&doc, "runtime") == "claude"
        && super::string(&doc, "group_id") == group
        && super::string(&doc, "actor_id") == actor
        && valid_session_id(&id))
    .then_some(id))
}

pub(crate) fn load(home: &HomeLayout, group: &str, actor: &Actor) -> io::Result<Option<Ownership>> {
    let file = path(home, group, &actor.id)?.with_extension("owner.json");
    let doc = match std::fs::read(&file) {
        Ok(raw) => serde_json::from_slice::<Ownership>(&raw).map_err(io::Error::other)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if doc.group_id != group || doc.actor_id != actor.id || doc.actor_created_at != actor.created_at
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Claude ownership does not match actor generation; leaving provider untouched",
        ));
    }
    Ok(Some(doc))
}

pub(crate) fn record(
    home: &HomeLayout,
    group: &str,
    actor_id: &str,
    config: &Path,
    cwd: &Path,
    session: &str,
    uncertain: bool,
) -> io::Result<()> {
    let group_doc = GroupStore::new(home.clone())?.load(group)?;
    let actor = group_doc
        .actors
        .iter()
        .find(|a| a.id == actor_id)
        .ok_or_else(|| io::Error::other("Claude owner actor is missing"))?;
    let doc = Ownership {
        group_id: group.into(),
        actor_id: actor_id.into(),
        actor_created_at: actor.created_at.clone(),
        session_id: session.into(),
        config_dir: config.canonicalize()?,
        workspace: cwd.canonicalize()?,
        uncertain,
    };
    cccc_core::fs::write_json(
        &path(home, group, actor_id)?.with_extension("owner.json"),
        &doc,
    )
}

pub(crate) fn remove(home: &HomeLayout, group: &str, actor: &str) -> io::Result<()> {
    match std::fs::remove_file(path(home, group, actor)?.with_extension("owner.json")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub(crate) fn note_probe(home: &HomeLayout, group: &str, actor: &str) -> io::Result<()> {
    let mut doc = read(home, group, actor)?;
    doc.insert(
        "last_resume_attempt_id".into(),
        serde_json::json!(uuid::Uuid::new_v4().simple().to_string()),
    );
    write(home, group, actor, &doc)
}
