//! The retry cap on repeated oversized context answers (spec item 3).
//!
//! Timestamps are handed in, so these tests move the clock instead of sleeping.

use super::*;

fn group() -> &'static str {
    "g-request-cap"
}

fn over() -> usize {
    512 * 1024
}

fn under() -> usize {
    4 * 1024
}

const BUDGET: usize = 256 * 1024;

#[test]
fn three_oversize_answers_trip_the_cap_and_the_fourth_is_refused() {
    reset();
    let start = Instant::now();
    for attempt in 0..MAX_OVERSIZE_ANSWERS {
        let now = start + Duration::from_millis(500 * attempt as u64);
        assert!(
            refusal(group(), "lab-dsh", now).is_none(),
            "attempt {attempt} must still be served"
        );
        record(group(), "lab-dsh", over(), BUDGET, now);
    }
    let refusal = refusal(group(), "lab-dsh", start + Duration::from_secs(2))
        .expect("the cap holds after three oversized answers");
    assert_eq!(refusal.code, "context_too_large");
    assert!(
        refusal.message.contains("limit/cursor") || refusal.message.contains("summary=true"),
        "the refusal must say how to get an answer that fits: {}",
        refusal.message
    );
    assert!(
        refusal.message.len() < 400,
        "the refusal is one bounded line, not another payload: {}",
        refusal.message.len()
    );
    let retry_after = refusal.details["retry_after_ms"]
        .as_u64()
        .expect("retry_after_ms");
    assert!(
        retry_after > 0,
        "a refused caller is told when to come back"
    );
}

#[test]
fn an_answer_that_fits_clears_the_record() {
    reset();
    let start = Instant::now();
    record(group(), "lab-hermes9", over(), BUDGET, start);
    record(
        group(),
        "lab-hermes9",
        over(),
        BUDGET,
        start + Duration::from_millis(200),
    );
    // A paged answer from the same caller means it is paging now.
    record(
        group(),
        "lab-hermes9",
        under(),
        BUDGET,
        start + Duration::from_millis(400),
    );
    assert!(
        refusal(group(), "lab-hermes9", start + Duration::from_millis(500)).is_none(),
        "paging must not be punished by the cap"
    );
    // ... and the next oversized answer starts a fresh count.
    record(
        group(),
        "lab-hermes9",
        over(),
        BUDGET,
        start + Duration::from_millis(600),
    );
    assert!(refusal(group(), "lab-hermes9", start + Duration::from_millis(700)).is_none());
}

#[test]
fn slow_retries_past_the_window_do_not_trip_the_cap() {
    reset();
    let start = Instant::now();
    for attempt in 0..(MAX_OVERSIZE_ANSWERS + 2) {
        let now = start + ATTEMPT_WINDOW * (attempt as u32 + 1);
        record(group(), "slow-caller", over(), BUDGET, now);
        assert!(
            refusal(group(), "slow-caller", now).is_none(),
            "one unbounded answer per window is a session, not a loop"
        );
    }
}

#[test]
fn the_cooldown_ends() {
    reset();
    let start = Instant::now();
    for attempt in 0..MAX_OVERSIZE_ANSWERS {
        record(
            group(),
            "lab-hermes2",
            over(),
            BUDGET,
            start + Duration::from_millis(100 * attempt as u64),
        );
    }
    assert!(refusal(group(), "lab-hermes2", start + Duration::from_secs(1)).is_some());
    assert!(
        refusal(
            group(),
            "lab-hermes2",
            start + COOLDOWN + Duration::from_secs(2)
        )
        .is_none(),
        "the breaker is a pause, not a ban"
    );
}

#[test]
fn a_request_without_a_caller_is_never_refused() {
    reset();
    let start = Instant::now();
    for attempt in 0..(MAX_OVERSIZE_ANSWERS * 3) {
        record(
            group(),
            "",
            over(),
            BUDGET,
            start + Duration::from_millis(attempt as u64),
        );
    }
    assert!(refusal(group(), "", start + Duration::from_secs(1)).is_none());
}

#[test]
fn one_group_tripping_the_cap_does_not_refuse_another() {
    reset();
    let start = Instant::now();
    for attempt in 0..MAX_OVERSIZE_ANSWERS {
        record(
            "g-one",
            "lab-hermes4",
            over(),
            BUDGET,
            start + Duration::from_millis(attempt as u64),
        );
    }
    assert!(refusal("g-one", "lab-hermes4", start + Duration::from_secs(1)).is_some());
    assert!(
        refusal("g-two", "lab-hermes4", start + Duration::from_secs(1)).is_none(),
        "the cap is per group, not per actor across every board"
    );
}
