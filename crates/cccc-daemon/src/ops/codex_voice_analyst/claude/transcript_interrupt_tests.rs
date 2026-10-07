use super::{tests::harness, *};

fn user(prompt_id: &str, text: &str) -> Value {
    json!({"type":"user","sessionId":"session-1","promptId":prompt_id,"message":{"content":text}})
}

#[test]
fn both_interrupt_markers_release_the_turn_and_accept_the_next_delivery() {
    for marker in [INTERRUPTION_MARKER, TOOL_INTERRUPTION_MARKER] {
        let (mut state, mut events) = harness();
        state
            .ingest(&user("initial", "inspect"), None)
            .expect("ingest transcript record");
        events.try_recv().expect("receive queued event");
        state
            .ingest(&user("initial", marker), None)
            .expect("ingest transcript record");
        events.try_recv().expect("receive queued event"); // completed message
        let ended = events.try_recv().expect("receive queued event");
        assert_eq!(ended.message["params"]["turn"]["status"], "cancelled");
        assert!(state.active_turn_id().is_none());
        assert_eq!(
            state
                .ingest(
                    &user("next", "continue"),
                    Some(PendingPrompt {
                        delegation_id: "next-delivery",
                        text: "continue",
                        turn_id: "next-turn"
                    })
                )
                .expect("ingest transcript record")
                .as_deref(),
            Some("next-turn")
        );
        assert_eq!(
            events
                .try_recv()
                .expect("receive queued event")
                .requested_delegation_id
                .as_deref(),
            Some("next-delivery")
        );
    }
}

#[test]
fn interruption_of_an_admitted_followup_cancels_the_same_turn() {
    for native in [false, true] {
        let (mut state, mut events) = harness();
        state
            .ingest(&user("initial", "inspect"), None)
            .expect("ingest transcript record");
        events.try_recv().expect("receive queued event");
        let outcome = state
            .ingest_with_native(
                &user("followup", "new constraint"),
                None,
                native.then_some(PendingNativeInput {
                    delegation_id: "followup-delivery",
                    text: "new constraint",
                }),
            )
            .expect("complete ingest with native in fixture");
        if native {
            assert_eq!(outcome, IngestOutcome::Native("followup-delivery".into()));
            assert_eq!(
                events.try_recv().expect("receive queued event").message["params"]["turnId"],
                "claude-initial"
            );
        }
        state
            .ingest(&user("followup", INTERRUPTION_MARKER), None)
            .expect("ingest transcript record");
        events.try_recv().expect("receive queued event");
        let ended = events.try_recv().expect("receive queued event");
        assert_eq!(ended.message["params"]["turn"]["id"], "claude-initial");
        assert_eq!(ended.message["params"]["turn"]["status"], "cancelled");
        assert!(state.active_turn_id().is_none());

        // Old or unadmitted prompt ids cannot cancel a subsequent turn.
        state
            .ingest(&user("next", "next request"), None)
            .expect("ingest transcript record");
        assert!(
            state
                .ingest(&user("followup", TOOL_INTERRUPTION_MARKER), None)
                .is_err()
        );
        assert_eq!(state.active_turn_id(), Some("claude-next"));
    }
}

fn tool_turn() -> TranscriptState {
    let (mut state, _) = harness();
    state.ingest(&user("initial", "inspect"), None).unwrap();
    state
        .ingest(
            &json!({"type":"assistant","sessionId":"session-1","uuid":"assistant-current",
        "message":{"content":[{"type":"tool_use","id":"call-current","name":"Bash","input":{}}]}}),
            None,
        )
        .unwrap();
    state
}

fn result() -> Value {
    json!({"type":"user","sessionId":"session-1","uuid":"result-current",
        "parentUuid":"assistant-current","promptId":"resume-minted",
        "message":{"content":[{"type":"tool_result","tool_use_id":"call-current","is_error":true,"content":"Interrupted"}]}})
}

fn result_interruption() -> Value {
    json!({"type":"user","sessionId":"session-1","parentUuid":"result-current",
        "promptId":"resume-minted","message":{"content":[{"type":"text","text":TOOL_INTERRUPTION_MARKER}]}})
}

#[test]
fn queued_resume_tool_interruption_owns_the_result_parent_without_adopting_its_prompt_id() {
    let mut state = tool_turn();
    state.ingest(&result(), None).unwrap();
    assert!(
        !state
            .active
            .as_ref()
            .unwrap()
            .prompt_ids
            .contains("resume-minted")
    );
    state.ingest(&result_interruption(), None).unwrap();
    assert!(state.active_turn_id().is_none());
    state.ingest(&user("next", "continue"), None).unwrap();
    assert_eq!(state.active_turn_id(), Some("claude-next"));
}

#[test]
fn tool_result_anchor_requires_both_owned_tool_and_exact_parent_and_nonblank_uuid() {
    for variant in [
        "wrong-tool",
        "missing-tool",
        "wrong-parent",
        "missing-parent",
        "blank-parent",
        "missing-uuid",
        "blank-uuid",
    ] {
        let mut state = tool_turn();
        let mut record = result();
        match variant {
            "wrong-tool" => record["message"]["content"][0]["tool_use_id"] = json!("foreign-call"),
            "missing-tool" => {
                record["message"]["content"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("tool_use_id");
            }
            "wrong-parent" => record["parentUuid"] = json!("foreign-assistant"),
            "missing-parent" => {
                record.as_object_mut().unwrap().remove("parentUuid");
            }
            "blank-parent" => record["parentUuid"] = json!(" "),
            "missing-uuid" => {
                record.as_object_mut().unwrap().remove("uuid");
            }
            "blank-uuid" => record["uuid"] = json!(" "),
            _ => unreachable!(),
        }
        state.ingest(&record, None).unwrap();
        assert!(
            state.ingest(&result_interruption(), None).is_err(),
            "{variant}"
        );
        assert_eq!(state.active_turn_id(), Some("claude-initial"));
    }
}

#[test]
fn filtered_tool_results_cannot_advance_the_interruption_anchor() {
    for variant in ["foreign-session", "sidechain", "meta"] {
        let mut state = tool_turn();
        let mut record = result();
        match variant {
            "foreign-session" => record["sessionId"] = json!("other-session"),
            "sidechain" => record["isSidechain"] = json!(true),
            "meta" => record["isMeta"] = json!(true),
            _ => unreachable!(),
        }
        let observed = state.ingest(&record, None);
        assert_eq!(observed.is_err(), variant == "foreign-session");
        assert!(
            state.ingest(&result_interruption(), None).is_err(),
            "{variant}"
        );
    }
}

#[test]
fn consumed_tool_ids_and_old_turns_cannot_advance_the_interruption_anchor() {
    let mut state = tool_turn();
    state.ingest(&result(), None).unwrap();
    let mut repeated = result();
    repeated["parentUuid"] = json!("result-current");
    repeated["uuid"] = json!("repeated-result");
    state.ingest(&repeated, None).unwrap();
    let mut marker = result_interruption();
    marker["parentUuid"] = json!("repeated-result");
    assert!(state.ingest(&marker, None).is_err());
    state.ingest(&result_interruption(), None).unwrap();
    assert!(state.ingest(&result_interruption(), None).is_err());
    state.ingest(&user("next", "continue"), None).unwrap();
    state.ingest(&result(), None).unwrap();
    assert!(state.ingest(&result_interruption(), None).is_err());
    assert_eq!(state.active_turn_id(), Some("claude-next"));
}
