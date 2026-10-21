use cccc_contracts::ActorRole;
use std::io;

use crate::GroupDoc;
use crate::actors::effective_role;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorAction {
    List,
    Add,
    Remove,
    Update,
    Start,
    Stop,
    Restart,
}

pub fn require_actor(
    group: &GroupDoc,
    by: &str,
    action: ActorAction,
    target: &str,
) -> io::Result<()> {
    let who = by.trim();
    if who.is_empty() || who == "user" || who == "system" {
        return Ok(());
    }
    // The Foreman is the first visible Actor (`effective_role`), so removing it
    // silently promotes the next Actor: the role changes without anyone
    // deciding it, and Mail still addressed to the old Foreman starts failing.
    // A removal that moves the role belongs to the user (or to an explicit
    // handoff, which does not exist yet), never to an Actor - not even the
    // Foreman itself.
    if action == ActorAction::Remove && effective_role(group, target) == Some(ActorRole::Foreman) {
        return Err(io::Error::other(format!(
            "permission denied: {who} may not remove the foreman ({target}); \
             only the user may remove it, or an actor must hand the role off first"
        )));
    }
    match (effective_role(group, who), action) {
        (Some(_), ActorAction::List) => Ok(()),
        (
            Some(ActorRole::Foreman),
            ActorAction::Add | ActorAction::Start | ActorAction::Stop | ActorAction::Restart,
        ) => Ok(()),
        // A Foreman removes peers, never itself: a self-removal is a silent
        // succession with nobody having decided it (2026-10-21, a Foreman
        // removed itself and the role fell to the next Actor for 90 minutes).
        (Some(ActorRole::Foreman), ActorAction::Remove)
            if effective_role(group, target) == Some(ActorRole::Peer) =>
        {
            Ok(())
        }
        (Some(ActorRole::Peer), ActorAction::Restart) => Ok(()),
        (Some(ActorRole::Peer), ActorAction::Stop) if target == who => Ok(()),
        (Some(_), ActorAction::Update) => Err(io::Error::other(
            "actor.update is restricted to user-facing ports",
        )),
        (Some(_), _) => Err(io::Error::other(format!("permission denied: {who}"))),
        (None, _) => Err(io::Error::other(format!("unknown actor: {who}"))),
    }
}

pub fn require_group(group: &GroupDoc, by: &str) -> io::Result<()> {
    let who = by.trim();
    if who.is_empty() || who == "user" || who == "system" {
        return Ok(());
    }
    match effective_role(group, who) {
        Some(ActorRole::Foreman) => Ok(()),
        Some(ActorRole::Peer) => Err(io::Error::other(format!("permission denied: {who}"))),
        None => Err(io::Error::other(format!("unknown actor: {who}"))),
    }
}

pub fn require_group_member(group: &GroupDoc, by: &str) -> io::Result<()> {
    let who = by.trim();
    if who.is_empty() || who == "user" || who == "system" {
        return Ok(());
    }
    effective_role(group, who)
        .map(|_| ())
        .ok_or_else(|| io::Error::other(format!("unknown actor: {who}")))
}

pub fn require_inbox(group: &GroupDoc, by: &str, target: &str) -> io::Result<()> {
    let who = by.trim();
    if who.is_empty() || who == "user" || who == "system" {
        return Ok(());
    }
    match effective_role(group, who) {
        Some(ActorRole::Foreman) => Ok(()),
        Some(ActorRole::Peer) if who == target => Ok(()),
        Some(ActorRole::Peer) => Err(io::Error::other(format!("permission denied: {who}"))),
        None => Err(io::Error::other(format!("unknown actor: {who}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The Foreman is the first visible Actor, so Actor order is the whole
    /// fixture.
    fn group(ids: &[&str]) -> GroupDoc {
        let actors: Vec<serde_json::Value> = ids.iter().map(|id| json!({"id": id})).collect();
        serde_json::from_value(json!({
            "v": 1,
            "group_id": "g_test",
            "title": "permissions",
            "created_at": "2026-10-21T00:00:00Z",
            "updated_at": "2026-10-21T00:00:00Z",
            "actors": actors,
        }))
        .expect("group fixture")
    }

    fn removal(group: &GroupDoc, by: &str, target: &str) -> io::Result<()> {
        require_actor(group, by, ActorAction::Remove, target)
    }

    #[test]
    fn the_foreman_cannot_remove_itself() {
        let group = group(&["lead", "peer1"]);
        let error = removal(&group, "lead", "lead").expect_err("foreman removed itself");
        assert!(
            error.to_string().contains("may not remove the foreman"),
            "{error}"
        );
    }

    #[test]
    fn a_peer_cannot_remove_itself() {
        let group = group(&["lead", "peer1"]);
        assert!(removal(&group, "peer1", "peer1").is_err());
    }

    #[test]
    fn a_peer_cannot_remove_the_foreman() {
        let group = group(&["lead", "peer1"]);
        assert!(removal(&group, "peer1", "lead").is_err());
    }

    #[test]
    fn the_foreman_still_removes_a_peer() {
        let group = group(&["lead", "peer1"]);
        assert!(removal(&group, "lead", "peer1").is_ok());
    }

    #[test]
    fn the_user_may_remove_the_foreman() {
        let group = group(&["lead", "peer1"]);
        assert!(removal(&group, "user", "lead").is_ok());
    }

    #[test]
    fn a_peer_may_still_stop_itself() {
        let group = group(&["lead", "peer1"]);
        assert!(require_actor(&group, "peer1", ActorAction::Stop, "peer1").is_ok());
    }
}
