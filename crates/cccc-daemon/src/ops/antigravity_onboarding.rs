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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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

/// What the delivery worker should do while first-run setup is unresolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Gate {
    /// Onboarding is complete (or cannot be proven incomplete): deliver.
    Proceed,
    /// The wizard visibly owns the terminal: an operator may be completing it
    /// right now, so hold the delivery for a later attempt.
    Defer,
    /// Setup is incomplete and no wizard is on screen: settle Failed.
    Fail,
}

/// Text the `agy` first-run wizard shows while it owns the terminal. These
/// strings come from the wizard's own screens; the completed prompt shows
/// none of them.
const WIZARD_MARKERS: &[&str] = &[
    "Choose your color scheme",
    "Terms of Service & Data Use",
    "Welcome to the",
];

/// How long one delivery attempt watches for the wizard before deciding the
/// terminal is not showing it. Covers `agy` startup before the wizard draws.
const WIZARD_WAIT: Duration = Duration::from_secs(10);
const SCREEN_POLL: Duration = Duration::from_millis(200);
const SCREEN_HISTORY_BYTES: usize = 256 * 1024;

/// The gate for a running native `agy` PTY. While `onboarding.json` still
/// reports first-run setup pending, the delivery waits only as long as the
/// wizard is actually on screen — an operator stepping through it inline is
/// not failed. Once the wizard is gone (or never appeared within the startup
/// window) the delivery settles Failed with [`REQUIRED_REASON`].
pub(super) fn delivery_gate(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    cancelled: &AtomicBool,
) -> Gate {
    gate_with_wait(home, group, actor, cancelled, WIZARD_WAIT)
}

fn gate_with_wait(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    cancelled: &AtomicBool,
    wait: Duration,
) -> Gate {
    if !setup_required(home, group, actor) {
        return Gate::Proceed;
    }
    let deadline = Instant::now() + wait;
    loop {
        // The operator can finish the wizard while this attempt watches.
        if !setup_required(home, group, actor) {
            return Gate::Proceed;
        }
        if let Some(text) = screen_text(home, &group.group_id, &actor.id)
            && wizard_visible(&text)
        {
            return Gate::Defer;
        }
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Gate::Fail;
        }
        std::thread::sleep(SCREEN_POLL);
    }
}

/// Rendered visible text of the actor's retained terminal output.
fn screen_text(home: &HomeLayout, group_id: &str, actor_id: &str) -> Option<String> {
    let page = super::terminal_history_source::retained(
        home,
        group_id,
        actor_id,
        SCREEN_HISTORY_BYTES,
    )
    .ok()?;
    let text = super::terminal_text::render(&page.data, true);
    (!text.trim().is_empty()).then_some(text)
}

fn wizard_visible(text: &str) -> bool {
    WIZARD_MARKERS.iter().any(|marker| text.contains(marker))
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

    #[test]
    fn wizard_markers_match_only_first_run_screens() {
        assert!(wizard_visible(
            "Welcome to the \nAntigravity CLI\nChoose your color scheme:"
        ));
        assert!(wizard_visible("Terms of Service & Data Use"));
        assert!(!wizard_visible("agy prompt\n> "));
        assert!(!wizard_visible("Gemini 4 Argon low\nauto"));
    }

    /// A live PTY showing the wizard defers; the same pending onboarding with
    /// an ordinary prompt fails once the startup window closes.
    #[cfg(unix)]
    fn gate_fixture(
        screen: &str,
    ) -> (tempfile::TempDir, HomeLayout, GroupDoc, Actor, PathBuf) {
        use cccc_contracts::RunnerKind;
        use std::collections::BTreeMap;
        let temp = tempfile::tempdir().expect("tempdir");
        let cccc_home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let provider_home = temp.path().join("provider");
        std::fs::create_dir_all(&provider_home).expect("provider home");
        let (_, group) = group_with_actor(&cccc_home, actor_with_home(&provider_home));
        let script = temp.path().join("screen.py");
        std::fs::write(
            &script,
            "import sys,tty,time\ntty.setraw(0)\nsys.stdout.write(sys.argv[1])\nsys.stdout.flush()\ntime.sleep(60)\n",
        )
        .expect("fixture script");
        cccc_runtime::start(cccc_runtime::LaunchSpec {
            group_id: group.group_id.clone(),
            actor_id: "agy".into(),
            runner: RunnerKind::Pty,
            command: vec![
                "python3".into(),
                script.to_string_lossy().into_owned(),
                screen.to_owned(),
            ],
            cwd: temp.path().to_path_buf(),
            env: BTreeMap::new(),
            cols: 120,
            rows: 40,
        })
        .expect("start fixture");
        // The PTY needs a moment to record its first frame.
        std::thread::sleep(Duration::from_millis(300));
        let actor = group.actors[0].clone();
        (temp, cccc_home, group, actor, provider_home)
    }

    #[cfg(unix)]
    #[test]
    fn wizard_on_screen_defers_pending_onboarding() {
        let (_temp, home, group, actor, provider_home) = gate_fixture(
            "\u{1b}[?2004h\u{1b}[2J\u{1b}[HWelcome to the \nAntigravity CLI\nChoose your color scheme:",
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_eq!(
                gate_with_wait(
                    &home,
                    &group,
                    &actor,
                    &AtomicBool::new(false),
                    Duration::from_secs(2)
                ),
                Gate::Defer,
                "the visible wizard holds the delivery"
            );
            // Completing setup while the wizard text is still on screen lets
            // the next attempt through: onboarding.json is authoritative.
            write_onboarding(&provider_home, true);
            assert_eq!(
                gate_with_wait(
                    &home,
                    &group,
                    &actor,
                    &AtomicBool::new(false),
                    Duration::from_millis(100)
                ),
                Gate::Proceed
            );
        }));
        cccc_runtime::stop(&group.group_id, &actor.id).expect("stop fixture");
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }

    #[cfg(unix)]
    #[test]
    fn pending_onboarding_without_the_wizard_fails() {
        let (_temp, home, group, actor, _provider_home) =
            gate_fixture("\u{1b}[?2004h\u{1b}[2J\u{1b}[Hagy prompt\n> ");
        assert_eq!(
            gate_with_wait(
                &home,
                &group,
                &actor,
                &AtomicBool::new(false),
                Duration::from_millis(500)
            ),
            Gate::Fail,
            "no wizard on screen means the delivery cannot wait forever"
        );
        cccc_runtime::stop(&group.group_id, &actor.id).expect("stop fixture");
    }
}
