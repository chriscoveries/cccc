//! Install as crates/cccc-contracts/src/actor_memory_policy_red.rs, under actor.
//! Uses only API present in both 953ad1fa and 4583e5fa.
use super::{Actor, ActorRuntime, RuntimeMode};
use serde_json::{Value, json};

const RUNTIMES: [ActorRuntime; 22] = [
    ActorRuntime::Amp,
    ActorRuntime::Antigravity,
    ActorRuntime::Auggie,
    ActorRuntime::Claude,
    ActorRuntime::Cline,
    ActorRuntime::Codex,
    ActorRuntime::Deepseek,
    ActorRuntime::Copilot,
    ActorRuntime::Cursor,
    ActorRuntime::Devin,
    ActorRuntime::Kiro,
    ActorRuntime::Kilo,
    ActorRuntime::Droid,
    ActorRuntime::Grok,
    ActorRuntime::Hermes,
    ActorRuntime::Kimi,
    ActorRuntime::Opencode,
    ActorRuntime::Pi,
    ActorRuntime::WebModel,
    ActorRuntime::ChatgptDots,
    ActorRuntime::GrokWebModel,
    ActorRuntime::Custom,
];

fn lane_documents() -> Vec<Value> {
    let mut documents = Vec::new();
    for runtime in RUNTIMES {
        for mode in [RuntimeMode::Default, RuntimeMode::Acp] {
            if mode.validate(runtime).is_err() {
                continue;
            }
            let mut actor = Actor::new("memory-policy-worker");
            actor.runtime = runtime;
            actor.runtime_mode = mode;
            actor.normalize_runtime_constraints();
            documents.push(serde_json::to_value(actor).expect("serialize lane"));
        }
    }
    documents
}

#[test]
fn every_new_lane_exposes_the_4096_mib_default() {
    for lane in lane_documents() {
        assert_eq!(
            lane.get("memory_limit_mb"),
            Some(&json!(4096)),
            "new lane must have the owner default: {lane}"
        );
    }
}

#[test]
fn legacy_actors_without_a_limit_acquire_the_same_default() {
    for mut lane in lane_documents() {
        lane.as_object_mut()
            .expect("actor object")
            .remove("memory_limit_mb");
        let loaded: Actor = serde_json::from_value(lane).expect("legacy actor");
        let saved = serde_json::to_value(loaded).expect("save legacy actor");
        assert_eq!(saved.get("memory_limit_mb"), Some(&json!(4096)), "{saved}");
    }
}

#[test]
fn explicit_off_and_overrides_survive_two_save_load_cycles_in_every_lane() {
    for lane in lane_documents() {
        for limit in [0_u32, 256, 4096, 8192] {
            let mut saved = lane.clone();
            saved["memory_limit_mb"] = json!(limit);
            for _ in 0..2 {
                let loaded: Actor = serde_json::from_value(saved).expect("configured actor");
                saved = serde_json::to_value(loaded).expect("save configured actor");
                assert_eq!(
                    saved.get("memory_limit_mb"),
                    Some(&json!(limit)),
                    "explicit zero must remain distinguishable from an omitted default: {saved}"
                );
            }
        }
    }
}
