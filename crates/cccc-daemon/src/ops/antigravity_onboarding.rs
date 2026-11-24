//! Antigravity (`agy`) first-run onboarding gate for native PTY delivery.
//!
//! A fresh provider `HOME` makes `agy` show a one-time wizard (colour scheme,
//! terms) that owns the terminal input. Text pasted while the wizard is up is
//! consumed by the wizard instead of the conversation, so the delivery looked
//! accepted while the task never arrived. CCCC settles such deliveries Failed
//! with an actionable reason instead of silently eating them; completing the
//! wizard once (run `agy` interactively in the actor's `HOME`) writes
//! `onboarding.json` and delivery proceeds normally.

use cccc_contracts::{Actor, ActorRuntime};
use cccc_core::{GroupDoc, HomeLayout};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Reason recorded on deliveries settled Failed while first-run setup is
/// incomplete. Names the one-time interactive `agy` run that completes it.
pub(super) const REQUIRED_REASON: &str = "Antigravity first-run setup required: run `agy` once interactively in this actor's HOME to finish onboarding";

/// `agy` records first-run completion here, relative to the effective `HOME`.
const ONBOARDING_PATH: &str = ".gemini/antigravity-cli/cache/onboarding.json";

/// True for the native `agy` TUI surface. ACP mode uses the managed headless
/// adapter, which does not pass through this wizard.
pub(super) fn gates_native_pty(actor: &Actor) -> bool {
    actor.runtime == ActorRuntime::Antigravity
        && !actor.runtime.is_headless_acp(actor.runtime_mode)
}

/// The `HOME` the launched `agy` process resolves, mirroring launch
/// precedence: the effective actor environment first (profile + secrets, the
/// same resolution `actor.start` uses), then the daemon's own environment,
/// which the PTY child inherits.
fn effective_home(home: &HomeLayout, group: &GroupDoc, actor: &Actor) -> Option<PathBuf> {
    effective_home_from(&launch_env(home, group, actor))
}

fn launch_env(home: &HomeLayout, group: &GroupDoc, actor: &Actor) -> BTreeMap<String, String> {
    // Best effort: a profile or secret that cannot be resolved must not mask
    // the actor's own environment, which is what a plain launch would use.
    let resolved =
        crate::ops::actor_profile_runtime::resolve(home, actor).unwrap_or_else(|_| actor.clone());
    crate::ops::actor_secrets::effective_values(home, &group.group_id, &resolved)
        .unwrap_or_else(|_| resolved.env.clone())
}

fn effective_home_from(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(|key| env.get(*key))
        .find(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

fn onboarding_file(home_dir: &Path) -> PathBuf {
    home_dir.join(ONBOARDING_PATH)
}

/// `agy` writes `onboardingComplete: true` only after the operator finishes
/// the wizard; absence or any other value means first-run setup is pending.
fn onboarding_complete(home_dir: &Path) -> bool {
    cccc_core::fs::read_json::<serde_json::Value>(&onboarding_file(home_dir))
        .ok()
        .and_then(|value| {
            value
                .get("onboardingComplete")
                .and_then(serde_json::Value::as_bool)
        })
        == Some(true)
}

/// True when first-run setup cannot be confirmed complete for this actor.
/// An undeterminable `HOME` fails open: the gate only fires on positive
/// evidence that onboarding is unfinished.
pub(super) fn setup_required(home: &HomeLayout, group: &GroupDoc, actor: &Actor) -> bool {
    effective_home(home, group, actor).is_some_and(|home_dir| !onboarding_complete(&home_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::GroupStore;

    fn actor_with_home(home_dir: &Path) -> Actor {
        let mut actor = Actor::new("agy");
        actor.runtime = ActorRuntime::Antigravity;
        actor.env.insert(
            "HOME".into(),
            home_dir.to_string_lossy().into_owned(),
        );
        actor
    }

    fn group_with_actor(home: &HomeLayout, actor: Actor) -> (GroupStore, GroupDoc) {
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("onboarding", "").expect("group");
        group.actors.push(actor);
        store.save(&group).expect("save");
        (store, group)
    }

    fn write_onboarding(home_dir: &Path, complete: bool) {
        let path = onboarding_file(home_dir);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(
            path,
            format!(
                "{{\"consumerOnboardingComplete\":{complete},\"enterpriseOnboardingComplete\":false,\"onboardingComplete\":{complete}}}"
            ),
        )
        .expect("onboarding file");
    }

    #[test]
    fn missing_onboarding_file_requires_first_run_setup() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cccc_home = HomeLayout::from_path(temp.path().join("cccc")).expect("home");
        let provider_home = temp.path().join("provider");
        std::fs::create_dir_all(&provider_home).expect("provider home");
        let (_, group) = group_with_actor(&cccc_home, actor_with_home(&provider_home));

        assert!(setup_required(&cccc_home, &group, &group.actors[0]));
    }

    #[test]
    fn incomplete_onboarding_flag_requires_first_run_setup() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cccc_home = HomeLayout::from_path(temp.path().join("cccc")).expect("home");
        let provider_home = temp.path().join("provider");
        write_onboarding(&provider_home, false);
        let (_, group) = group_with_actor(&cccc_home, actor_with_home(&provider_home));

        assert!(setup_required(&cccc_home, &group, &group.actors[0]));
    }

    #[test]
    fn completed_onboarding_allows_delivery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cccc_home = HomeLayout::from_path(temp.path().join("cccc")).expect("home");
        let provider_home = temp.path().join("provider");
        write_onboarding(&provider_home, true);
        let (_, group) = group_with_actor(&cccc_home, actor_with_home(&provider_home));

        assert!(!setup_required(&cccc_home, &group, &group.actors[0]));
    }

    #[test]
    fn home_comes_from_the_actor_environment() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cccc_home = HomeLayout::from_path(temp.path().join("cccc")).expect("home");
        let fresh = temp.path().join("fresh");
        let done = temp.path().join("done");
        write_onboarding(&done, true);
        let mut actor = actor_with_home(&done);
        actor.env.insert("HOME".into(), fresh.to_string_lossy().into_owned());
        let (_, group) = group_with_actor(&cccc_home, actor);

        assert!(
            setup_required(&cccc_home, &group, &group.actors[0]),
            "the actor's own HOME decides, not another directory's onboarding state"
        );
    }

    #[test]
    fn non_antigravity_actors_are_never_gated() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cccc_home = HomeLayout::from_path(temp.path().join("cccc")).expect("home");
        let provider_home = temp.path().join("provider");
        let mut actor = actor_with_home(&provider_home);
        actor.runtime = ActorRuntime::Claude;
        let (_, group) = group_with_actor(&cccc_home, actor);

        assert!(!gates_native_pty(&group.actors[0]));
    }

    #[test]
    fn antigravity_acp_mode_is_not_gated() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cccc_home = HomeLayout::from_path(temp.path().join("cccc")).expect("home");
        let provider_home = temp.path().join("provider");
        let mut actor = actor_with_home(&provider_home);
        actor.runtime_mode = cccc_contracts::RuntimeMode::Acp;
        let (_, group) = group_with_actor(&cccc_home, actor);

        assert!(!gates_native_pty(&group.actors[0]));
    }
}
