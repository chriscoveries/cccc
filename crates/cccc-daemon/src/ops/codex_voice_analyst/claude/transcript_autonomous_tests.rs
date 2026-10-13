use super::{tests::harness, *};

fn user(prompt_id: &str, text: &str) -> Value {
    json!({"type":"user","sessionId":"session-1","promptId":prompt_id,"message":{"content":text}})
}

fn turn_duration() -> Value {
    json!({"type":"system","sessionId":"session-1","subtype":"turn_duration","isMeta":false})
}

/// The hidden input Claude Code records when another Claude session messages this one. Claude
/// answers it in a turn of its own, without any terminal or CCCC input.
fn cross_session_message(prompt_id: &str) -> Value {
    json!({
        "type":"user","sessionId":"session-1","promptId":prompt_id,"isMeta":true,
        "origin":{"kind":"peer","from":"bridge:session_peer","name":"PEER"},
        "message":{"role":"user","content":[{"type":"text","text":
            "Another Claude session sent a message:\n<cross-session-message from=\"bridge:session_peer\">Thanks</cross-session-message>"}]}
    })
}

fn assistant(content: Value) -> Value {
    json!({"type":"assistant","sessionId":"session-1","message":{"content":content}})
}

fn drain(events: &mut broadcast::Receiver<AnalystEvent>) -> Vec<Value> {
    std::iter::from_fn(|| events.try_recv().ok().map(|event| event.message)).collect()
}

#[test]
fn reply_to_a_cross_session_message_is_tracked_instead_of_invalidating_the_session() {
    let (mut state, mut events) = harness();
    // An earlier CCCC-delivered turn completes normally.
    state
        .ingest(&user("delivered", "status?"), None)
        .expect("user");
    state
        .ingest(&assistant(json!([{"type":"text","text":"done"}])), None)
        .expect("assistant");
    state.ingest(&turn_duration(), None).expect("settle");
    drain(&mut events);

    // A peer session's message starts an autonomous turn that uses a tool and answers.
    assert_eq!(
        state
            .ingest(&cross_session_message("peer-prompt"), None)
            .expect("hidden input"),
        None
    );
    assert!(state.active_turn_id().is_none());
    assert!(events.try_recv().is_err());
    state
        .ingest(
            &assistant(json!([{"type":"tool_use","id":"tool-1","name":"TaskUpdate","input":{}}])),
            None,
        )
        .expect("autonomous assistant output");
    assert_eq!(state.active_turn_id(), Some("claude-peer-prompt"));
    state
        .ingest(
            &json!({
                "type":"user","sessionId":"session-1","promptId":"peer-prompt",
                "message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"ok"}]}
            }),
            None,
        )
        .expect("tool result");
    state
        .ingest(&assistant(json!([{"type":"text","text":"noted"}])), None)
        .expect("assistant text");
    state.ingest(&turn_duration(), None).expect("settle");

    let messages = drain(&mut events);
    assert_eq!(messages[0]["method"], "turn/started");
    assert_eq!(messages[0]["params"]["turn"]["id"], "claude-peer-prompt");
    let completed = messages.last().expect("completed turn");
    assert_eq!(completed["method"], "turn/completed");
    assert_eq!(completed["params"]["turn"]["status"], "completed");
    assert!(state.active_turn_id().is_none());

    // The session keeps accepting CCCC deliveries afterwards.
    assert_eq!(
        state
            .ingest(
                &user("next", "continue"),
                Some(PendingPrompt {
                    delegation_id: "next-delivery",
                    text: "continue",
                    turn_id: "next-turn",
                }),
            )
            .expect("next delivery")
            .as_deref(),
        Some("next-turn")
    );
}

#[test]
fn autonomous_turn_can_be_interrupted_by_its_hidden_prompt_id() {
    let (mut state, mut events) = harness();
    state
        .ingest(&cross_session_message("peer-prompt"), None)
        .expect("hidden input");
    state
        .ingest(&assistant(json!([{"type":"text","text":"working"}])), None)
        .expect("assistant");
    state
        .ingest(&user("peer-prompt", INTERRUPTION_MARKER), None)
        .expect("interruption of the autonomous turn");
    let ended = drain(&mut events).pop().expect("completed turn");
    assert_eq!(ended["params"]["turn"]["status"], "cancelled");
    assert!(state.active_turn_id().is_none());
}

#[test]
fn hidden_input_without_a_following_reply_opens_no_turn() {
    let (mut state, mut events) = harness();
    state
        .ingest(&cross_session_message("ignored"), None)
        .expect("hidden input");
    state.ingest(&turn_duration(), None).expect("settle");
    assert!(state.active_turn_id().is_none());
    assert!(events.try_recv().is_err());
    // The stale hidden prompt is not reused for a later CCCC turn.
    state
        .ingest(&user("delivered", "next"), None)
        .expect("user");
    assert_eq!(state.active_turn_id(), Some("claude-delivered"));
}

#[test]
fn hidden_input_during_a_turn_joins_it() {
    let (mut state, mut events) = harness();
    state
        .ingest(&user("delivered", "inspect"), None)
        .expect("user");
    state
        .ingest(&cross_session_message("peer-prompt"), None)
        .expect("hidden input");
    state
        .ingest(&assistant(json!([{"type":"text","text":"both"}])), None)
        .expect("assistant");
    assert_eq!(state.active_turn_id(), Some("claude-delivered"));
    state
        .ingest(&user("peer-prompt", TOOL_INTERRUPTION_MARKER), None)
        .expect("interruption carrying the hidden prompt id");
    let ended = drain(&mut events).pop().expect("completed turn");
    assert_eq!(ended["params"]["turn"]["status"], "cancelled");
}

#[test]
fn output_without_a_turn_or_hidden_input_is_adopted_not_disconnected() {
    let (mut state, mut events) = harness();
    // Formerly failed closed ("without an active transcript turn") and killed
    // the session. Now the output is adopted as a self-started turn with a
    // diagnostic, and the session survives (WICK claude 08:46Z).
    state
        .ingest(&assistant(json!([{"type":"text","text":"orphan"}])), None)
        .expect("orphan output is adopted");
    assert_eq!(state.active_turn_id(), Some("claude-untracked-1"));
    let started = events.try_recv().expect("turn started");
    assert_eq!(started.message["method"], "turn/started");
    assert_eq!(
        started.message["params"]["turn"]["id"],
        "claude-untracked-1"
    );
}

#[test]
fn autonomous_output_cannot_swallow_a_pending_controlled_prompt() {
    let (mut state, mut events) = harness();
    let pending = || PendingPrompt {
        delegation_id: "controlled-delivery",
        text: "inspect this",
        turn_id: "controlled-turn",
    };
    // The control request was accepted, but its exact user echo is not yet
    // visible. A hidden peer input starts competing work in that interval.
    state
        .ingest(&cross_session_message("peer-prompt"), Some(pending()))
        .expect("hidden input alone does not establish a turn");
    let error = state
        .ingest(
            &assistant(json!([{"type":"text","text":"peer answer"}])),
            Some(pending()),
        )
        .expect_err("competing output must expose ambiguous control acceptance");
    assert!(error.to_string().contains("turn ownership is ambiguous"));
    assert!(state.active_turn_id().is_none());
    assert!(
        events.try_recv().is_err(),
        "no competing result can own the delivery"
    );
}

#[test]
fn settled_hidden_input_does_not_excuse_later_orphan_output() {
    let (mut state, _) = harness();
    state
        .ingest(&cross_session_message("stale"), None)
        .expect("hidden input");
    state.ingest(&turn_duration(), None).expect("settle");
    // The stale hidden prompt was cleared by the settle, so the orphan is
    // adopted unattributed — but still adopted, never a disconnect.
    state
        .ingest(&assistant(json!([{"type":"text","text":"orphan"}])), None)
        .expect("orphan output is adopted");
    assert_eq!(state.active_turn_id(), Some("claude-untracked-1"));
}

/// A scheduled (cron/loop) prompt fires while idle: hidden input, then Claude's own turn.
#[test]
fn scheduled_prompt_turn_is_tracked() {
    let (mut state, mut events) = harness();
    state
        .ingest(
            &json!({"type":"system","sessionId":"session-1","subtype":"scheduled_task_fire"}),
            None,
        )
        .expect("fire marker");
    state
        .ingest(
            &json!({
                "type":"user","sessionId":"session-1","promptId":"scheduled-1","isMeta":true,
                "promptSource":"system","turnOrigin":"scheduled",
                "message":{"role":"user","content":"Periodic check: report anything stalled."}
            }),
            None,
        )
        .expect("scheduled hidden input");
    state
        .ingest(
            &assistant(json!([{"type":"text","text":"nothing stalled"}])),
            None,
        )
        .expect("scheduled turn output");
    state.ingest(&turn_duration(), None).expect("settle");
    let messages = drain(&mut events);
    assert_eq!(messages[0]["params"]["turn"]["id"], "claude-scheduled-1");
    assert_eq!(
        messages.last().expect("completed")["params"]["turn"]["status"],
        "completed"
    );
}

#[test]
fn resume_acknowledgement_consumes_the_hidden_resume_prompt() {
    let (mut state, _) = harness();
    state
        .ingest(
            &json!({
                "type":"user","sessionId":"session-1","promptId":"resume","isMeta":true,
                "message":{"content":"Continue from where you left off."}
            }),
            None,
        )
        .expect("hidden resume prompt");
    state
        .ingest(
            &json!({
                "type":"assistant","sessionId":"session-1","isApiErrorMessage":false,
                "message":{"model":"<synthetic>","content":[{"type":"text","text":"No response requested."}]}
            }),
            None,
        )
        .expect("resume acknowledgement");
    // The acknowledgement consumed the hidden prompt; a later orphan is
    // still adopted as a self-started turn, never a disconnect.
    state
        .ingest(&assistant(json!([{"type":"text","text":"orphan"}])), None)
        .expect("orphan output is adopted");
    assert_eq!(state.active_turn_id(), Some("claude-untracked-1"));
}

#[test]
fn hidden_input_from_another_session_is_not_remembered() {
    let (mut state, _) = harness();
    let mut foreign = cross_session_message("foreign");
    foreign["sessionId"] = json!("another-session");
    state.ingest(&foreign, None).expect("foreign hidden input");
    // The foreign prompt is not remembered, so the orphan is adopted
    // unattributed — but still adopted, never a disconnect.
    state
        .ingest(&assistant(json!([{"type":"text","text":"orphan"}])), None)
        .expect("orphan output is adopted");
    assert_eq!(state.active_turn_id(), Some("claude-untracked-1"));
}

/// Two hidden inputs can land before Claude answers (e.g. a held then denied delivery notice).
/// Either one may own an interruption of the turn Claude starts for them.
#[test]
fn every_pending_hidden_input_belongs_to_the_turn_it_starts() {
    let (mut state, mut events) = harness();
    state
        .ingest(&cross_session_message("first"), None)
        .expect("first hidden input");
    state
        .ingest(&cross_session_message("second"), None)
        .expect("second hidden input");
    assert!(state.hidden_input_pending());
    state
        .ingest(
            &assistant(json!([{"type":"text","text":"answering"}])),
            None,
        )
        .expect("assistant");
    assert!(!state.hidden_input_pending());
    assert_eq!(state.active_turn_id(), Some("claude-first"));
    state
        .ingest(&user("first", INTERRUPTION_MARKER), None)
        .expect("interruption carrying the earlier hidden prompt id");
    let ended = drain(&mut events).pop().expect("completed turn");
    assert_eq!(ended["params"]["turn"]["status"], "cancelled");
}

/// The exact WICK 08:46:37 record sequence (cwf#116 comment 6120984998): a
/// queued message interrupts a running tool. Before the fix, the dequeued
/// prompt was absorbed into the dying turn, the interrupt cancelled it, and
/// the answering assistant output had no turn, so the tracker disconnected
/// and the lane died. Now the dequeued prompt opens its own turn at the
/// interrupt and the answer lands in it: no disconnect, no lost lane.
#[test]
fn queued_prompt_interrupting_a_tool_opens_its_turn() {
    let (mut state, mut events) = harness();
    // The long Bash tool turn is running.
    state
        .ingest(&user("prompt-long", "run a long command"), None)
        .expect("user");
    assert_eq!(state.active_turn_id(), Some("claude-prompt-long"));
    state
        .ingest(
            &assistant(json!([{"type":"tool_use","id":"tool-9","name":"Bash","input":{"command":"sleep 200"}}])),
            None,
        )
        .expect("tool call");
    // The queued message arrives mid-turn and is absorbed as a correction.
    state
        .ingest(&user("prompt-queued", "please also summarize"), None)
        .expect("dequeued prompt absorbed");
    assert_eq!(state.active_turn_id(), Some("claude-prompt-long"));
    // The running tool reports, then the queued prompt interrupts it.
    state
        .ingest(
            &json!({
                "type":"user","sessionId":"session-1","promptId":"prompt-long",
                "message":{"content":[{"type":"tool_result","tool_use_id":"tool-9","content":"done"}]}
            }),
            None,
        )
        .expect("tool result");
    state
        .ingest(&user("prompt-queued", TOOL_INTERRUPTION_MARKER), None)
        .expect("interrupt");
    // The interrupt cancelled the tool turn AND opened the dequeued prompt's
    // own turn instead of leaving the next output orphaned.
    assert_eq!(state.active_turn_id(), Some("claude-prompt-queued"));
    let started = drain(&mut events)
        .into_iter()
        .find(|message| {
            message["method"] == "turn/started"
                && message["params"]["turn"]["id"] == "claude-prompt-queued"
        })
        .expect("new turn started for the dequeued prompt");
    assert_eq!(started["params"]["turn"]["id"], "claude-prompt-queued");
    // The answer to the queued prompt lands in its turn: session survives.
    state
        .ingest(
            &assistant(json!([
                {"type":"text","text":"summary"},
                {"type":"tool_use","id":"tool-10","name":"Bash","input":{}}
            ])),
            None,
        )
        .expect("answer to the queued prompt");
    assert_eq!(state.active_turn_id(), Some("claude-prompt-queued"));
    state.ingest(&turn_duration(), None).expect("settle");
    assert!(state.active_turn_id().is_none());
}

/// REDO finding 1: a stashed correction answered by later output must not
/// reopen as a phantom turn on an ordinary cancel.
#[test]
fn answered_correction_is_not_reopened_by_a_later_cancel() {
    let (mut state, mut events) = harness();
    state
        .ingest(&user("prompt-long", "run a long command"), None)
        .expect("user");
    assert_eq!(state.active_turn_id(), Some("claude-prompt-long"));
    // A mid-turn correction is absorbed (stashed).
    state
        .ingest(&user("prompt-note", "also note this"), None)
        .expect("correction absorbed");
    // The turn answers: the correction is consumed, stash cleared.
    state
        .ingest(&assistant(json!([{"type":"text","text":"noted"}])), None)
        .expect("assistant");
    assert_eq!(state.active_turn_id(), Some("claude-prompt-long"));
    // An ordinary cancel now ends the turn WITHOUT opening a phantom turn
    // for the long-answered correction.
    state
        .ingest(&user("prompt-note", TOOL_INTERRUPTION_MARKER), None)
        .expect("cancel");
    assert!(state.active_turn_id().is_none());
    let ended = drain(&mut events)
        .into_iter()
        .find(|message| message["method"] == "turn/completed")
        .expect("cancelled turn");
    assert_eq!(ended["params"]["turn"]["status"], "cancelled");
}

/// REDO finding 2: orphan output with a pending controlled delivery is
/// adopted (text preserved, never dropped); the delivery's echo then settles
/// the adopted turn and opens the controlled one, so the receipt completes
/// instead of hanging to timeout.
#[test]
fn orphan_adopted_then_controlled_echo_settles_and_opens() {
    let (mut state, mut events) = harness();
    let pending = || PendingPrompt {
        delegation_id: "controlled-delivery",
        text: "inspect this",
        turn_id: "controlled-turn",
    };
    // Spontaneous output arrives before the delivery's echo: adopted, since
    // dropping it would be silent loss.
    assert_eq!(
        state
            .ingest(
                &assistant(json!([{"type":"text","text":"spontaneous"}])),
                Some(pending())
            )
            .expect("orphan adopted"),
        None
    );
    assert_eq!(state.active_turn_id(), Some("claude-untracked-1"));
    // The exact echo settles the adopted turn and opens the controlled one:
    // text preserved in history, receipt completes exactly.
    assert_eq!(
        state
            .ingest(&user("prompt-1", "inspect this"), Some(pending()),)
            .expect("echo")
            .as_deref(),
        Some("controlled-turn")
    );
    assert_eq!(state.active_turn_id(), Some("controlled-turn"));
    let settled = drain(&mut events)
        .into_iter()
        .find(|message| {
            message["method"] == "turn/completed"
                && message["params"]["turn"]["id"] == "claude-untracked-1"
        })
        .expect("adopted turn settled");
    assert_eq!(settled["params"]["turn"]["status"], "cancelled");
}

/// REDO ownership: a cancel marker for another prompt must not reopen a
/// fresh absorbed correction as a phantom successor.
#[test]
fn cancel_for_another_prompt_does_not_reopen_a_fresh_correction() {
    let (mut state, mut events) = harness();
    state
        .ingest(&user("prompt-orig", "original work"), None)
        .expect("user");
    assert_eq!(state.active_turn_id(), Some("claude-prompt-orig"));
    // A fresh correction is absorbed (stashed) with no output after it.
    state
        .ingest(&user("prompt-extra", "extra note"), None)
        .expect("correction absorbed");
    // The cancel marker owns the ORIGINAL prompt, not the stashed one.
    state
        .ingest(&user("prompt-orig", TOOL_INTERRUPTION_MARKER), None)
        .expect("cancel");
    assert!(state.active_turn_id().is_none());
    let ended = drain(&mut events)
        .into_iter()
        .find(|message| message["method"] == "turn/completed")
        .expect("cancelled turn");
    assert_eq!(ended["params"]["turn"]["status"], "cancelled");
}

/// REDO delivery accounting: nonmatching text while a controlled delivery
/// is pending fails closed (like the terminal race), so it cannot silently
/// absorb or strand the delivery.
#[test]
fn nonmatching_text_with_pending_controlled_fails_closed() {
    let (mut state, _) = harness();
    let pending = || PendingPrompt {
        delegation_id: "controlled-delivery",
        text: "inspect this",
        turn_id: "controlled-turn",
    };
    // An adopted turn holds the tracker (orphan adopted first).
    state
        .ingest(
            &assistant(json!([{"type":"text","text":"spontaneous"}])),
            None
        )
        .expect("orphan adopted");
    assert_eq!(state.active_turn_id(), Some("claude-untracked-1"));
    // Different text with the delivery pending: ambiguous, fail closed.
    let error = state
        .ingest(&user("prompt-other", "something else"), Some(pending()))
        .expect_err("nonmatching text must not absorb a pending delivery");
    assert!(error.to_string().contains("turn ownership is ambiguous"));
    assert_eq!(state.active_turn_id(), Some("claude-untracked-1"));
}
