//! Additional acceptance controls; all calls exist on the immutable RED source.
use super::*;
const MODAL: &str = "Switch model?\r\nYour next response will be slower and use more tokens\r\nThis conversation is cached for the current model. Switching to Opus 5.5 means the full history gets re-read on\r\nyour next message.\r\n❯ 1. Yes, switch to Opus 5.5\r\n  2. No, go back";
const ORDERS: [&str; 8] = [
    "\x1b[2J\x1b[H",
    "\x1b[H\x1b[2J",
    "\x1b[2J\x1b[3J\x1b[H",
    "\x1b[2J\x1b[H\x1b[3J",
    "\x1b[3J\x1b[2J\x1b[H",
    "\x1b[3J\x1b[H\x1b[2J",
    "\x1b[H\x1b[2J\x1b[3J",
    "\x1b[H\x1b[3J\x1b[2J",
];
fn paint(order: &str, text: &str) -> Vec<u8> {
    format!("\x1b[?2026h{order}{text}\x1b[?2026l").into_bytes()
}
fn evicted() -> Screen {
    let mut screen = Screen::default();
    screen
        .feed(&vec![b'x'; MAX_OUTPUT + 71])
        .expect("actual window eviction");
    assert_eq!(screen.output.len(), MAX_OUTPUT);
    assert!(screen.anchor.is_none());
    screen
}
#[test]
fn every_clear_scrollback_home_order_recovers_after_actual_eviction() {
    for order in ORDERS {
        let mut screen = evicted();
        screen
            .feed(&paint(order, MODAL))
            .expect("native complete paint");
        assert!(screen.cache_cost("Opus 5.5"), "order {order:?}");
        assert!(!screen.empty_prompt());
        screen
            .feed(&paint(order, "❯\r\n⏵⏵ bypass permissions on"))
            .expect("new current prompt");
        assert!(screen.empty_prompt(), "order {order:?}");
        assert!(!screen.cache_cost("Opus 5.5"));
    }
}
#[test]
fn reordered_recovery_survives_every_split_and_waits_for_the_completed_frame() {
    for order in ORDERS {
        let bytes = paint(order, MODAL);
        for split in 0..bytes.len() {
            let mut screen = evicted();
            screen.feed(&bytes[..split]).expect("partial stream");
            assert!(
                !screen.cache_cost("Opus 5.5"),
                "premature split {split}, {order:?}"
            );
            assert!(!screen.empty_prompt());
            screen.feed(&bytes[split..]).expect("remaining stream");
            assert!(
                screen.cache_cost("Opus 5.5"),
                "completed split {split}, {order:?}"
            );
        }
        let mut screen = evicted();
        for byte in bytes {
            screen.feed(&[byte]).expect("bytewise CSI and UTF8");
        }
        assert!(screen.cache_cost("Opus 5.5"));
    }
}
#[test]
fn scrollback_clear_never_substitutes_for_visible_display_clear() {
    for order in ["\x1b[3J\x1b[H", "\x1b[H\x1b[3J", "\x1b[3J"] {
        let mut screen = evicted();
        screen
            .feed(&paint(order, MODAL))
            .expect("not a recovery anchor");
        assert!(!screen.cache_cost("Opus 5.5"));
        assert!(!screen.empty_prompt());
    }
}
#[test]
fn partial_reset_tokens_never_combine_across_unrelated_synchronized_frames() {
    for (first, second) in [("\x1b[2J", "\x1b[H"), ("\x1b[H", "\x1b[2J")] {
        let mut screen = evicted();
        screen
            .feed(&paint(first, "old unrelated frame"))
            .expect("first frame");
        screen.feed(&paint(second, MODAL)).expect("second frame");
        assert!(!screen.cache_cost("Opus 5.5"));
        assert!(!screen.empty_prompt());
    }
}
#[test]
fn reordered_anchors_inside_streamed_osc_and_dcs_never_authorize() {
    for start in [b"\x1b]title;".as_slice(), b"\x1bPignored;".as_slice()] {
        for order in ORDERS {
            let mut screen = evicted();
            screen.feed(start).expect("control string start");
            for byte in paint(order, MODAL) {
                screen.feed(&[byte]).expect("split fake paint in string");
                assert!(!screen.cache_cost("Opus 5.5"));
                assert!(!screen.empty_prompt());
            }
            screen.feed(b"\x1b\\").expect("control string end");
            assert!(!screen.cache_cost("Opus 5.5"));
            screen
                .feed(&paint(order, MODAL))
                .expect("actual top-level recovery");
            assert!(screen.cache_cost("Opus 5.5"));
        }
    }
}
#[test]
fn current_unrelated_or_incomplete_frame_cannot_reuse_the_old_modal() {
    let mut screen = evicted();
    screen
        .feed(&paint("\x1b[H\x1b[2J", MODAL))
        .expect("current modal");
    assert!(screen.cache_cost("Opus 5.5"));
    screen
        .feed(&paint(
            "\x1b[3J\x1b[2J\x1b[H",
            "❯\r\n⏵⏵ bypass permissions on",
        ))
        .expect("new unrelated current frame");
    assert!(!screen.cache_cost("Opus 5.5"));
    assert!(screen.empty_prompt());
    screen
        .feed(b"\x1b[?2026h\x1b[H\x1b[2J")
        .expect("incomplete newest repaint");
    assert!(!screen.cache_cost("Opus 5.5"));
    assert!(!screen.empty_prompt());
}

#[test]
fn unrelated_escape_or_text_breaks_an_unfinished_reset_run() {
    for order in [
        "\x1b[H\x1bq\x1b[2J",
        "\x1b[2J\x1bq\x1b[H",
        "\x1b[Hstale text\x1b[2J",
    ] {
        let mut screen = evicted();
        screen
            .feed(&paint(order, MODAL))
            .expect("unrelated reset tokens");
        assert!(!screen.cache_cost("Opus 5.5"));
        assert!(!screen.empty_prompt());
    }
}
