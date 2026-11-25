//! Work ownership guards for an actor rename.
//!
//! The daemon's group write permit serializes native tasks and lifecycle requests.
//! Workflow writers outside the daemon use the advisory locks beside their stores.
//! Acquire those locks without waiting: a workflow writer may be holding one
//! while waiting for daemon IPC. Keep them until the rename is durable.
//!
//! The board uses `$CCCC_HOME/workflows/board/<group>/lock` and `ops.jsonl`.
//! Flow normally uses `$CCCC_HOME/workflows/flow/<group>/bin/merge-executor.lock`.
//! For an alternate flow root, register the group's absolute path in the
//! administrator-managed `$CCCC_HOME/workflows/flow-roots.json` JSON object.
//! A board-managed group with no standard flow root must have an explicit
//! entry. Use a null entry to declare that it has no external flow.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use cccc_core::context::ContextStore;
use cccc_core::{GroupDoc, HomeLayout, actors};
use serde::Deserialize;
use serde_json::Value;

use crate::dispatch::OpError;

#[derive(Debug)]
pub(super) struct RenameGuards {
    _board: File,
    _flows: Vec<File>,
}

pub(super) fn lock_and_check(
    home: &HomeLayout,
    group: &GroupDoc,
    actor_id: &str,
) -> Result<RenameGuards, OpError> {
    let matches_actor = |candidate: &str| actors::canonical_actor_id(group, candidate) == actor_id;
    let context = ContextStore::new(home.clone())
        .and_then(|store| store.load(&group.group_id))
        .map_err(OpError::io)?;
    if let Some(task) = context.tasks.iter().find(|task| {
        task.get("status").and_then(Value::as_str) == Some("active")
            && ["assignee", "handoff_to"].iter().any(|field| {
                task.get(*field)
                    .and_then(Value::as_str)
                    .is_some_and(&matches_actor)
            })
    }) {
        return Err(OpError::new(
            "actor_rename_task_active",
            format!(
                "actor {actor_id} holds active task {}",
                task.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("(unknown)")
            ),
        ));
    }

    let workflows = home.root().join("workflows");
    let board_dir = workflows.join("board").join(&group.group_id);
    // The lock also protects against a board writer creating its first record.
    let board = lock(&board_dir.join("lock"))?;
    let journal = board_dir.join("ops.jsonl");
    let journal_exists = exists(&journal)?;
    if !journal_exists && exists(&board_dir.join("state.json"))? {
        return Err(unavailable(
            &journal,
            "board cache exists without its journal",
        ));
    }
    if let Some(card) = board_claim(&journal, &matches_actor)? {
        return Err(OpError::new(
            "actor_rename_claim_held",
            format!("actor {actor_id} holds a claim on {card}"),
        ));
    }

    let mut flows = Vec::new();
    for root in flow_roots(&workflows, &group.group_id, journal_exists)? {
        let bin = root.join("bin");
        if !exists(&bin)? {
            return Err(unavailable(
                &bin,
                "configured flow root has no bin directory",
            ));
        }
        let flow = lock(&bin.join("merge-executor.lock"))?;
        if flow_holds_actor(&bin, &matches_actor)? {
            return Err(OpError::new(
                "actor_rename_flow_held",
                format!("actor {actor_id} holds a flow item"),
            ));
        }
        flows.push(flow);
    }
    Ok(RenameGuards {
        _board: board,
        _flows: flows,
    })
}

fn unavailable(path: &Path, error: impl std::fmt::Display) -> OpError {
    OpError::new(
        "actor_rename_workflow_unavailable",
        format!(
            "cannot verify workflow ownership at {}: {error}",
            path.display()
        ),
    )
}

fn exists(path: &Path) -> Result<bool, OpError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(unavailable(path, error)),
    }
}

fn lock(path: &Path) -> Result<File, OpError> {
    cccc_core::fs::try_exclusive_lock(path)
        .map_err(|error| unavailable(path, error))?
        .ok_or_else(|| {
            OpError::new(
                "actor_rename_workflow_busy",
                format!(
                    "workflow writer holds {}; retry after it finishes",
                    path.display()
                ),
            )
        })
}

/// Select both stores if an administrator registers an alternate flow root
/// while a standard root still exists. Neither can silently mask a holder.
fn flow_roots(
    workflows: &Path,
    group_id: &str,
    board_managed: bool,
) -> Result<BTreeSet<PathBuf>, OpError> {
    let standard = workflows.join("flow").join(group_id);
    let config = workflows.join("flow-roots.json");
    let registered: BTreeMap<String, Option<PathBuf>> = optional_json(&config)?.unwrap_or_default();
    let mut roots = BTreeSet::new();
    if exists(&standard)? {
        roots.insert(fs::canonicalize(&standard).map_err(|error| unavailable(&standard, error))?);
    }
    match registered.get(group_id) {
        Some(Some(root)) => {
            if !root.is_absolute() {
                return Err(unavailable(&config, "flow root must be an absolute path"));
            }
            roots.insert(fs::canonicalize(root).map_err(|error| unavailable(root, error))?);
        }
        Some(None) if !roots.is_empty() => {
            return Err(unavailable(
                &config,
                "flow root is registered as absent but the standard store exists",
            ));
        }
        None if roots.is_empty() && board_managed => {
            return Err(unavailable(
                &config,
                "register this group's flow root, or null if the group has no external flow",
            ));
        }
        _ => {}
    }
    Ok(roots)
}

#[derive(Deserialize)]
#[serde(tag = "rec")]
enum BoardRecord {
    #[serde(rename = "claim")]
    Claim {
        card_id: String,
        actor: String,
        state: ClaimState,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum ClaimState {
    Reserved,
    Active,
    Stalled,
    Released,
}

fn board_claim(
    path: &Path,
    matches_actor: &impl Fn(&str) -> bool,
) -> Result<Option<String>, OpError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound && !exists(path)? => return Ok(None),
        Err(error) => return Err(unavailable(path, error)),
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut held = BTreeSet::new();
    while reader
        .read_line(&mut line)
        .map_err(|error| unavailable(path, error))?
        != 0
    {
        if !line.ends_with('\n') {
            return Err(unavailable(
                path,
                "board journal ends with an incomplete record",
            ));
        }
        if !line.trim().is_empty()
            && let BoardRecord::Claim {
                card_id,
                actor,
                state,
            } = serde_json::from_str(&line).map_err(|error| unavailable(path, error))?
        {
            // The latest journal record for the card supersedes older claims.
            if matches_actor(&actor) && state != ClaimState::Released {
                held.insert(card_id);
            } else {
                held.remove(&card_id);
            }
        }
        line.clear();
    }
    Ok(held.into_iter().next())
}

#[derive(Deserialize)]
struct FlowItems {
    items: Vec<FlowItem>,
}

#[derive(Deserialize)]
struct FlowItem {
    id: String,
}

#[derive(Deserialize)]
struct FlowHolders {
    #[serde(default)]
    holders: BTreeMap<String, FlowLane>,
    #[serde(default)]
    queued: BTreeMap<String, FlowLane>,
}

#[derive(Deserialize)]
struct FlowLane {
    lane: String,
}

#[derive(Deserialize)]
struct LegacyFlowState {
    #[serde(default)]
    st: BTreeMap<String, LegacyFlowItem>,
}

#[derive(Deserialize)]
struct LegacyFlowItem {
    #[serde(default)]
    lane: String,
    state: String,
}

fn flow_holds_actor(bin: &Path, matches_actor: &impl Fn(&str) -> bool) -> Result<bool, OpError> {
    let inputs = bin.join("items.json");
    let items: FlowItems =
        optional_json(&inputs)?.ok_or_else(|| unavailable(&inputs, "missing flow items"))?;
    let holders: Option<FlowHolders> = optional_json(&bin.join("holders.json"))?;
    let state: Option<LegacyFlowState> = optional_json(&bin.join("state.json"))?;
    if let Some(holders) = holders {
        return Ok(holders.holders.keys().any(|lane| matches_actor(lane))
            || holders
                .holders
                .values()
                .chain(holders.queued.values())
                .any(|entry| matches_actor(&entry.lane)));
    }
    // Before the holders sidecar is first written, the flow runner restores
    // in-flight holders from its old state rows for known items.
    Ok(state.is_some_and(|state| {
        items.items.iter().any(|item| {
            state.st.get(&item.id).is_some_and(|row| {
                matches_actor(&row.lane)
                    && matches!(row.state.as_str(), "dispatched" | "stalled" | "blocked")
            })
        })
    }))
}

fn optional_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, OpError> {
    match fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw)
            .map(Some)
            .map_err(|error| unavailable(path, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound && !exists(path)? => Ok(None),
        Err(error) => Err(unavailable(path, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::GroupStore;
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, HomeLayout, GroupDoc) {
        let temp = tempfile::tempdir().expect("fixture");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let group = GroupStore::new(home.clone())
            .expect("store")
            .create("Workflow fixture", "")
            .expect("group");
        (temp, home, group)
    }

    fn write(path: &Path, raw: impl AsRef<[u8]>) {
        fs::create_dir_all(path.parent().expect("fixture path")).expect("fixture directory");
        fs::write(path, raw).expect("fixture data");
    }

    #[test]
    fn active_task_for_an_alias_refuses_rename() {
        let (_temp, home, mut group) = fixture();
        group
            .actor_aliases
            .insert("before".into(), "current".into());
        let task = json!({
            "op": "task.create",
            "title": "Open work",
            "status": "active",
            "assignee": "before"
        });
        ContextStore::new(home.clone())
            .expect("contexts")
            .sync(
                &group.group_id,
                &[task.as_object().expect("task").clone()],
                None,
                "user",
                false,
            )
            .expect("seed task");
        assert_eq!(
            lock_and_check(&home, &group, "current")
                .expect_err("active task")
                .code,
            "actor_rename_task_active"
        );
    }

    #[test]
    fn latest_board_claim_and_explicit_flow_registration_gate_rename() {
        let (_temp, home, mut group) = fixture();
        group
            .actor_aliases
            .insert("before".into(), "current".into());
        let workflows = home.root().join("workflows");
        let board_path = workflows.join("board").join(&group.group_id);
        let journal = board_path.join("ops.jsonl");
        let claim = json!({"rec":"claim","card_id":"card-one","actor":"before","state":"active"});
        let release =
            json!({"rec":"claim","card_id":"card-one","actor":"before","state":"released"});
        write(&journal, format!("{claim}\n"));
        assert_eq!(
            lock_and_check(&home, &group, "current")
                .expect_err("board claim")
                .code,
            "actor_rename_claim_held"
        );

        write(&journal, format!("{claim}\n{release}\n"));
        let error = lock_and_check(&home, &group, "current").expect_err("missing flow declaration");
        assert_eq!(error.code, "actor_rename_workflow_unavailable");
        assert!(error.message.contains("register this group's flow root"));

        let map = BTreeMap::from([(&group.group_id, Value::Null)]);
        write(
            &workflows.join("flow-roots.json"),
            serde_json::to_vec(&map).expect("flow declaration"),
        );
        let guards = lock_and_check(&home, &group, "current").expect("quiet registered group");
        assert_eq!(
            lock_and_check(&home, &group, "current")
                .expect_err("board lock held")
                .code,
            "actor_rename_workflow_busy"
        );
        drop(guards);
        lock_and_check(&home, &group, "current").expect("board lock released");
    }

    #[test]
    fn nonstandard_flow_store_blocks_queued_and_live_holders() {
        let (temp, home, group) = fixture();
        let root = temp.path().join("alternate-flow");
        let bin = root.join("bin");
        let registration = BTreeMap::from([(&group.group_id, &root)]);
        write(
            &home.root().join("workflows/flow-roots.json"),
            serde_json::to_vec(&registration).expect("flow registration"),
        );
        write(&bin.join("items.json"), br#"{"items":[{"id":"item-one"}]}"#);
        let holders = bin.join("holders.json");
        write(
            &holders,
            br#"{"holders":{},"queued":{"item-one":{"lane":"current"}}}"#,
        );
        assert_eq!(
            lock_and_check(&home, &group, "current")
                .expect_err("queued flow item")
                .code,
            "actor_rename_flow_held"
        );

        write(
            &holders,
            br#"{"holders":{"current":{"lane":"current"}},"queued":{}}"#,
        );
        assert_eq!(
            lock_and_check(&home, &group, "current")
                .expect_err("held flow item")
                .code,
            "actor_rename_flow_held"
        );

        write(&holders, br#"{"holders":{},"queued":{}}"#);
        let guards = lock_and_check(&home, &group, "current").expect("quiet flow");
        assert!(
            cccc_core::fs::try_exclusive_lock(&bin.join("merge-executor.lock"))
                .expect("flow lock")
                .is_none()
        );
        drop(guards);

        write(&holders, b"malformed");
        assert_eq!(
            lock_and_check(&home, &group, "current")
                .expect_err("unreadable flow")
                .code,
            "actor_rename_workflow_unavailable"
        );
    }

    #[test]
    fn standard_flow_without_sidecar_checks_legacy_inflight_rows() {
        let (_temp, home, group) = fixture();
        let bin = home
            .root()
            .join("workflows/flow")
            .join(&group.group_id)
            .join("bin");
        write(&bin.join("items.json"), br#"{"items":[{"id":"item-one"}]}"#);
        write(
            &bin.join("state.json"),
            br#"{"st":{"item-one":{"lane":"current","state":"dispatched"}}}"#,
        );
        assert_eq!(
            lock_and_check(&home, &group, "current")
                .expect_err("legacy flow item")
                .code,
            "actor_rename_flow_held"
        );
        write(&bin.join("holders.json"), br#"{"holders":{},"queued":{}}"#);
        lock_and_check(&home, &group, "current").expect("sidecar overrides stale state");
    }
}
