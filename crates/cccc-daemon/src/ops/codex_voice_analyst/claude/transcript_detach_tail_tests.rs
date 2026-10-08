use super::{tests::harness, *};

#[test]
fn detached_observer_ignores_late_old_tail_then_tracks_new_input() {
    let (mut state, mut events) = harness();
    state.fence_existing_turn_tail();
    for record in [
        json!({"type":"assistant","sessionId":"session-1","message":{"content":[{"type":"text","text":"old reply"}]}}),
        json!({"type":"user","sessionId":"session-1","message":{"content":[{"type":"tool_result","tool_use_id":"old-tool","content":"done"}]}}),
        json!({"type":"system","sessionId":"session-1","subtype":"turn_duration"}),
    ] {
        state
            .ingest(&record, None)
            .expect("historical tail is fenced");
    }
    assert!(events.try_recv().is_err());
    let pending = PendingPrompt {
        delegation_id: "delivery-2",
        text: "follow up",
        turn_id: "turn-2",
    };
    let input = json!({
        "type":"user", "sessionId":"session-1", "promptId":"prompt-2",
        "message":{"content":"follow up"}
    });
    assert_eq!(
        state
            .ingest(&input, Some(pending))
            .expect("new input correlated")
            .as_deref(),
        Some("turn-2")
    );
    let reply = json!({
        "type":"assistant", "sessionId":"session-1",
        "message":{"content":[{"type":"text","text":"new reply"}]}
    });
    state.ingest(&reply, None).expect("new reply projected");
    state
        .ingest(
            &json!({"type":"system","sessionId":"session-1","subtype":"turn_duration"}),
            None,
        )
        .expect("new turn settled");
    assert!(
        std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| event.message["params"]["delta"] == "new reply")
    );
    assert!(
        state
            .ingest(
                &json!({"type":"assistant","sessionId":"session-1","message":{"content":[]}}),
                None
            )
            .is_err(),
        "strict turn validation resumes after the new input"
    );
}

#[test]
fn detach_tail_fence_still_rejects_foreign_session_and_invalid_new_input() {
    let (mut state, _) = harness();
    state.fence_existing_turn_tail();
    assert!(
        state
            .ingest(
                &json!({"type":"assistant","sessionId":"another","message":{"content":[]}}),
                None
            )
            .is_err()
    );
    let invalid_input = json!({
        "type":"user", "sessionId":"session-1",
        "message":{"content":"new input without prompt id"}
    });
    assert!(state.ingest(&invalid_input, None).is_err());
}

#[test]
fn rv2_fence_preserves_self_started_turn_and_native_steering() {
    let (mut state, mut events) = harness();
    state.fence_existing_turn_tail();
    let primary = json!({"type":"user","sessionId":"session-1","promptId":"self-start","message":{"content":"human begins a turn"}});
    state
        .ingest_with_native(&primary, None, None)
        .expect("fixture");
    let turn = state.active_turn_id().expect("fixture").to_owned();
    assert_eq!(turn, "claude-self-start");
    let started = events.try_recv().expect("fixture");
    assert_eq!(started.message["method"], "turn/started");
    assert!(started.requested_delegation_id.is_none());
    let steering = json!({"type":"user","sessionId":"session-1","promptId":"correction","message":{"content":"steer existing turn"}});
    assert_eq!(
        state
            .ingest_with_native(
                &steering,
                None,
                Some(PendingNativeInput {
                    delegation_id: "native-d",
                    text: "steer existing turn"
                })
            )
            .expect("fixture"),
        IngestOutcome::Native("native-d".into())
    );
    assert_eq!(state.active_turn_id(), Some(turn.as_str()));
    let attached = events.try_recv().expect("fixture");
    assert_eq!(
        attached.requested_delegation_id.as_deref(),
        Some("native-d")
    );
    state.ingest(&json!({"type":"assistant","sessionId":"session-1","message":{"content":[{"type":"text","text":"answer"}]}}),None).expect("fixture");
    state
        .ingest(
            &json!({"type":"system","sessionId":"session-1","subtype":"turn_duration"}),
            None,
        )
        .expect("fixture");
    let projected: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
    assert!(
        projected
            .iter()
            .any(|e| e.message["params"]["delta"] == "answer")
    );
    assert_eq!(
        projected
            .iter()
            .filter(|e| e.message["method"] == "turn/completed")
            .count(),
        1
    );
    assert!(state.active_turn_id().is_none());
}

#[test]
fn rv2_fence_preserves_input_identity_and_pending_prompt_checks() {
    let (mut state, _) = harness();
    state.fence_existing_turn_tail();
    let foreign = json!({"type":"user","sessionId":"other-session","promptId":"foreign","message":{"content":"input"}});
    assert!(state.ingest(&foreign, None).is_err());
    let input = json!({"type":"user","sessionId":"session-1","promptId":"input","message":{"content":"unexpected input"}});
    assert!(
        state
            .ingest(
                &input,
                Some(PendingPrompt {
                    delegation_id: "controlled",
                    text: "expected input",
                    turn_id: "controlled-turn"
                })
            )
            .is_err()
    );
    assert!(state.active_turn_id().is_none());
    assert!(
        state
            .ingest(
                &json!({"type":"assistant","sessionId":"other-session","message":{"content":[]}}),
                None
            )
            .is_err()
    );
    state.ingest(&input, None).expect("fixture");
    assert_eq!(state.active_turn_id(), Some("claude-input"));
}

#[test]
fn rv2_fence_correlates_first_new_native_turn() {
    let (mut state, mut events) = harness();
    state.fence_existing_turn_tail();
    let input = json!({"type":"user","sessionId":"session-1","promptId":"native-start","message":{"content":"native input"}});
    assert_eq!(
        state
            .ingest_with_native(
                &input,
                None,
                Some(PendingNativeInput {
                    delegation_id: "native-start-d",
                    text: "native input"
                })
            )
            .expect("fixture"),
        IngestOutcome::Native("native-start-d".into())
    );
    assert_eq!(state.active_turn_id(), Some("claude-native-start"));
    assert_eq!(
        events
            .try_recv()
            .expect("fixture")
            .requested_delegation_id
            .as_deref(),
        Some("native-start-d")
    );
}
