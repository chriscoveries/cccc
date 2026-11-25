//! Recover a renamed actor's native ACP pin after a legacy daemon replaced it.
use super::*;
use cccc_contracts::{Actor, Event};
use cccc_core::{GroupDoc, actors, fs, ledger, owned_mcp};
use serde::{Deserialize, Serialize};
use std::{fs as std_fs, io};

#[cfg(test)]
mod tests;

#[derive(Deserialize, Serialize)]
struct Checkpoint {
    rename: Event,
    receipts: BTreeMap<String, Value>,
    primary: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mcp_locator: Option<String>,
}

/// Called under the rename guards, after intent publication and before any move.
/// Normal session writes never address this directory.
pub(in crate::ops) fn checkpoint(
    home: &HomeLayout,
    group: &GroupDoc,
    old: &str,
    new: &str,
    event: &Event,
) -> io::Result<()> {
    let group_id = &group.group_id;
    let file = checkpoint_path(home, group_id, new)?;
    if let Some(saved) = read_optional::<Checkpoint>(&file)? {
        if saved.rename != *event {
            return Err(io::Error::other(
                "ACP checkpoint belongs to a different rename",
            ));
        }
    } else {
        let mut receipts = BTreeMap::new();
        for runtime in RUNTIMES {
            if let Some(receipt) = read_optional(&receipt_path(home, group_id, old, runtime)?)? {
                receipts.insert(runtime_name(runtime).into(), receipt);
            }
        }
        if receipts.is_empty() {
            return Ok(());
        }
        let primary = read_optional::<Value>(&path(home, group_id, old)?)?.filter(|primary| {
            primary["actor_id"] == old
                && primary["runtime"]
                    .as_str()
                    .and_then(|runtime| receipts.get(runtime))
                    .and_then(provider_id)
                    .is_some_and(|id| primary["provider_session_id"] == id)
        });
        let mcp_locator = actors::find(group, old)
            .and_then(|actor| owned_mcp::binding_file(home, group, old, &actor.created_at).ok())
            .filter(|file| owned_mcp::Binding::read_file(file).is_ok())
            .and_then(|file| file.file_name()?.to_str().map(str::to_owned));
        let saved = Checkpoint {
            rename: event.clone(),
            receipts,
            primary,
            mcp_locator,
        };
        fs::write_secret_json(&file, &saved)?;
    }
    // A new checkpoint directory must itself survive a crash before a receipt move.
    #[cfg(unix)]
    {
        let directory = file.parent().expect("checkpoint directory");
        std_fs::File::open(directory)?.sync_all()?;
        std_fs::File::open(directory.parent().expect("group state directory"))?.sync_all()?;
    }
    Ok(())
}

struct LegacyReplacement {
    checkpoint: Checkpoint,
    receipt: Value,
    original_id: String,
    legacy_id: String,
}

/// Publish the normal owner binding at the original locator. The ordinary
/// resolver still refuses ambiguity unless this proves and archives the one
/// duplicate emitted by a legacy start of this same native ACP actor.
#[cfg(unix)]
pub(in crate::ops) fn reconcile_mcp_binding(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
) -> io::Result<()> {
    if !actor.runtime.is_headless_acp(actor.runtime_mode)
        || !RUNTIMES.contains(&actor.runtime)
        || !resume_enabled()
    {
        return Ok(());
    }
    let Some(replacement) = legacy_replacement(home, &group.group_id, &actor.id, actor.runtime)?
    else {
        return Ok(());
    };
    let checkpoint = &replacement.checkpoint;
    let Some(locator) = checkpoint.mcp_locator.as_deref() else {
        return Ok(());
    };
    if rename_source(home, group, actor, &checkpoint.rename)?.is_none() {
        return Ok(());
    }
    let Some((duplicate, binding)) =
        owned_mcp::legacy_rename_duplicate(home, group, &actor.id, &actor.created_at, locator)?
    else {
        return Ok(());
    };
    let digest = Sha256::digest(
        serde_json::to_vec(&(&checkpoint.rename.id, &binding)).map_err(io::Error::other)?,
    );
    let archive = checkpoint_path(home, &group.group_id, &actor.id)?
        .with_file_name(format!(".mcp-legacy-{digest:x}.json"));
    fs::write_secret_json(&archive, &binding)?;
    if owned_mcp::Binding::read_file(&duplicate)? != binding {
        return Err(io::Error::other(
            "legacy MCP locator changed during ACP switchback",
        ));
    }
    std_fs::remove_file(&duplicate)?;
    std_fs::File::open(duplicate.parent().expect("MCP locators directory"))?.sync_all()
}

pub(in crate::ops) struct LegacyRecovery {
    pub(in crate::ops) session_id: String,
    archive: PathBuf,
    legacy: Value,
    primary: Option<PrimaryRecovery>,
    replacement_history: Option<Value>,
}

struct PrimaryRecovery {
    path: PathBuf,
    original: Value,
    legacy: Option<Value>,
}

/// A legacy replacement must name this exact checkpointed conversation. An
/// explicit New Session clears the active receipt and leaves no such link.
pub(in crate::ops) fn recover_legacy(
    home: &HomeLayout,
    group_id: &str,
    actor: &str,
    runtime: ActorRuntime,
    cwd: &Path,
    command: &[String],
    environment: &BTreeMap<String, String>,
) -> io::Result<Option<LegacyRecovery>> {
    if !resume_enabled() || environment.get("CCCC_ACTOR_ID").map(String::as_str) != Some(actor) {
        return Ok(None);
    }
    let Some(LegacyReplacement {
        checkpoint: saved,
        receipt: legacy,
        original_id,
        legacy_id,
    }) = legacy_replacement(home, group_id, actor, runtime)?
    else {
        return Ok(None);
    };
    let (identity, parts) = launch(cwd, command, environment);
    if legacy["identity"]["workspace_path"] != identity["workspace_path"]
        || legacy["launch_parts"]["v"] != LaunchParts::VERSION
        || legacy["launch_parts"]["rest"].as_str() != Some(parts.rest.as_str())
    {
        return Ok(None);
    }

    let group = GroupStore::new(home.clone())?.load(group_id)?;
    let Some(member) = actors::find(&group, actor) else {
        return Ok(None);
    };
    let Some(old) = rename_source(home, &group, member, &saved.rename)? else {
        return Ok(None);
    };
    let runtime_name = runtime_name(runtime);
    let original = &saved.receipts[runtime_name];

    // The checkpoint may still carry the launch identity of an earlier alias
    // if the actor was renamed more than once without an intervening start.
    let mut previous_environment = environment.clone();
    for alias in actors::actor_aliases(&group, actor) {
        previous_environment.insert("CCCC_ACTOR_ID".into(), alias.into());
        let (identity, parts) = launch(cwd, command, &previous_environment);
        if let Some(session_id) = resumable_id(original, &identity, &parts)? {
            let archive_key = Sha256::digest(
                serde_json::to_vec(&(&saved.rename.id, &legacy)).map_err(io::Error::other)?,
            );
            let archive = checkpoint_path(home, group_id, actor)?
                .with_file_name(format!(".legacy-{archive_key:x}.json"));
            let primary = paired_primary(
                path(home, group_id, actor)?,
                saved.primary,
                old,
                actor,
                &original_id,
                &legacy_id,
                runtime_name,
            )?;
            return Ok(Some(LegacyRecovery {
                session_id,
                archive,
                legacy,
                primary,
                replacement_history: original.get("replaced").cloned(),
            }));
        }
    }
    Ok(None)
}

fn legacy_replacement(
    home: &HomeLayout,
    group: &str,
    actor: &str,
    runtime: ActorRuntime,
) -> io::Result<Option<LegacyReplacement>> {
    let Some(checkpoint) = read_optional::<Checkpoint>(&checkpoint_path(home, group, actor)?)?
    else {
        return Ok(None);
    };
    let Some(original) = checkpoint.receipts.get(runtime_name(runtime)) else {
        return Ok(None);
    };
    let Some(receipt) = read_optional::<Value>(&receipt_path(home, group, actor, runtime)?)? else {
        return Ok(None);
    };
    let (Some(original_id), Some(legacy_id)) = (provider_id(original), provider_id(&receipt))
    else {
        return Ok(None);
    };
    if original["v"] != 1
        || original["attempted"] != true
        || receipt["v"] != 1
        || receipt.get(RENAME_AWARE).is_some()
        || legacy_id == original_id
        || receipt["replaced"]["previous"]["provider_id"].as_str() != Some(original_id)
        || receipt["identity"]["workspace_path"] != original["identity"]["workspace_path"]
    {
        return Ok(None);
    }
    let original_id = original_id.to_owned();
    let legacy_id = legacy_id.to_owned();
    Ok(Some(LegacyReplacement {
        checkpoint,
        receipt,
        original_id,
        legacy_id,
    }))
}

fn rename_source<'a>(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    event: &'a Event,
) -> io::Result<Option<&'a str>> {
    let Some(old) = event.data.get("old_actor_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let generation = actors::generation_identity(actor);
    if event.kind != "actor.rename"
        || event.group_id != group.group_id
        || event.data.get("new_actor_id").and_then(Value::as_str) != Some(actor.id.as_str())
        || event.data.get("actor_generation").and_then(Value::as_str) != Some(generation.as_str())
        || group.actor_aliases.get(old) != Some(&actor.id)
    {
        return Ok(None);
    }
    let path = GroupStore::new(home.clone())?.ledger_path(&group.group_id)?;
    Ok((ledger::find_event(&path, &event.id)?.as_ref() == Some(event)).then_some(old))
}

impl LegacyRecovery {
    /// Publish the pin only after the provider confirms it loaded the original.
    pub(in crate::ops) fn record(
        self,
        path: &Path,
        identity: &Value,
        parts: &LaunchParts,
        id: &str,
        resumed: bool,
    ) -> io::Result<()> {
        if !resumed || self.session_id != id {
            return Err(io::Error::other(
                "provider did not resume the checkpointed ACP session",
            ));
        }
        let legacy_primary = self
            .primary
            .as_ref()
            .and_then(|primary| primary.legacy.as_ref());
        let archive = match read_optional::<Value>(&self.archive)? {
            None => json!({"acp": self.legacy, "primary": legacy_primary}),
            Some(saved)
                if saved["acp"] == self.legacy
                    && legacy_primary.is_none_or(|primary| saved["primary"] == *primary) =>
            {
                saved
            }
            Some(_) => return Err(io::Error::other("ACP recovery archive contents differ")),
        };
        fs::write_secret_json(&self.archive, &archive)?;
        if let Some(primary) = self.primary {
            fs::write_secret_json(&primary.path, &primary.original)?;
        }
        let mut restored = receipt(identity, parts, id, resumed);
        if let Some(history) = self.replacement_history {
            restored["replaced"] = history;
        }
        fs::write_secret_json(path, &restored)
    }
}

fn paired_primary(
    path: PathBuf,
    original: Option<Value>,
    old: &str,
    actor: &str,
    original_id: &str,
    legacy_id: &str,
    runtime: &str,
) -> io::Result<Option<PrimaryRecovery>> {
    let Some(mut original) = original.filter(|value| {
        value["actor_id"] == old
            && value["provider_session_id"] == original_id
            && value["runtime"] == runtime
    }) else {
        return Ok(None);
    };
    original["actor_id"] = json!(actor);
    let legacy = read_optional::<Value>(&path)?;
    if legacy.as_ref().is_some_and(|receipt| {
        receipt["actor_id"] != actor
            || receipt["provider_session_id"] != legacy_id
            || receipt["runtime"] != runtime
            || receipt["transport"] != original["transport"]
    }) {
        return Ok(None);
    }
    Ok(Some(PrimaryRecovery {
        path,
        original,
        legacy,
    }))
}

fn runtime_name(runtime: ActorRuntime) -> &'static str {
    super::super::super::codex_voice_analyst::native_acp_name(runtime)
}

fn checkpoint_path(home: &HomeLayout, group: &str, actor: &str) -> io::Result<PathBuf> {
    let receipt = path(home, group, actor)?;
    Ok(GroupStore::new(home.clone())?
        .state_dir(group)?
        .join("actor_rename_acp")
        .join(receipt.file_name().expect("receipt file name")))
}

fn read_optional<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match std_fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => fs::read_json(path).map(Some),
        Ok(_) => Err(io::Error::other("ACP rename state is not a regular file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
