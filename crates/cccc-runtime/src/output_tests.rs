use super::OutputBuffer;

#[test]
fn tiny_writes_keep_all_bytes_without_one_allocation_per_read() {
    let mut output = OutputBuffer::with_capacity_at(128 * 1024, 40);
    let mut expected = Vec::new();
    for i in 0..200_000_u32 {
        let byte = b'a' + (i % 26) as u8;
        output.push(&[byte]);
        expected.push(byte);
    }
    let retained = &expected[expected.len() - 128 * 1024..];
    let page = output.page(None, retained.len());
    assert_eq!(page.data.as_bytes(), retained);
    assert_eq!(
        page.start_cursor,
        40 + (expected.len() - retained.len()) as u64
    );
    assert_eq!(page.end_cursor, 40 + expected.len() as u64);
    let allocated = output.chunks.capacity() * std::mem::size_of::<Vec<u8>>()
        + output.chunks.iter().map(Vec::capacity).sum::<usize>();
    assert!(
        allocated <= 140 * 1024,
        "tiny writes retain {allocated} bytes for a 128KiB history"
    );
}

#[test]
fn oversized_write_keeps_only_a_bounded_allocation_for_the_tail() {
    let mut output = OutputBuffer::with_capacity_at(1024, 42);
    output.push(&vec![b'x'; 1024 * 1024]);
    let allocated = output.chunks.capacity() * std::mem::size_of::<Vec<u8>>()
        + output.chunks.iter().map(Vec::capacity).sum::<usize>()
        + output.mode_probe.capacity();
    assert!(
        allocated <= 12 * 1024,
        "an oversized write retains {allocated} allocated bytes"
    );
    let page = output.page(None, 1024);
    assert_eq!(page.data, "x".repeat(1024));
    assert_eq!(page.end_cursor, 42 + 1024 * 1024);
    assert_eq!(page.start_cursor, page.end_cursor - 1024);
}

#[test]
fn block_boundaries_partial_eviction_and_trim_keep_exact_raw_cursors() {
    let mut output = OutputBuffer::with_capacity_at(8193, 42);
    let mut expected = Vec::new();
    let mut end = 42_u64;
    for length in [4095, 2, 4096, 1, 4095, 33, 8192, 16384, 7] {
        let data: Vec<u8> = (0..length).map(|i| (i % 251) as u8).collect();
        output.push(&data);
        end += data.len() as u64;
        expected.extend_from_slice(&data);
        expected = expected[expected.len().saturating_sub(8193)..].to_vec();
        let page = output.raw_retained_since(None);
        assert_eq!(page.data, expected);
        assert_eq!(page.end_cursor, end);
        assert_eq!(page.start_cursor, end - expected.len() as u64);
        let tail = output.raw_page_since(end - 5, 3);
        assert_eq!(tail.data, expected[expected.len() - 5..expected.len() - 2]);
    }
    output.trim_to(4097);
    expected = expected[expected.len() - 4097..].to_vec();
    output.push(b"after trim");
    expected.extend_from_slice(b"after trim");
    assert_eq!(output.raw_retained_since(None).data, expected);
    output.clear();
    output.push(b"after clear");
    let page = output.raw_retained_since(None);
    assert_eq!(page.data, b"after clear");
    assert_eq!(page.start_cursor, end + b"after trim".len() as u64);
}

#[test]
fn mode_probe_observes_discarded_prefix_and_split_modes_in_large_writes() {
    let mut output = OutputBuffer::with_capacity_at(16, 0);
    let mut data = b"\x1b[?2004h".to_vec();
    data.extend(std::iter::repeat_n(b'x', 16_384));
    data.extend_from_slice(b"\x1b[?20");
    output.push(&data);
    assert!(output.bracketed_paste_enabled());
    output.push(b"04l");
    assert!(!output.bracketed_paste_enabled());
}

#[test]
fn history_uses_absolute_cursors() {
    let mut output = OutputBuffer::default();
    output.push(b"hello");
    output.push(b" world");
    let page = output.page(None, 5);
    assert_eq!(page.data, "world");
    assert_eq!(page.start_cursor, 6);
    assert_eq!(page.end_cursor, 11);
}

#[test]
fn history_can_continue_from_a_previous_session_cursor() {
    let mut output = OutputBuffer::with_capacity_at(1024, 42);
    output.push(b"next");
    let page = output.retained_page();
    assert_eq!(page.data, "next");
    assert_eq!(page.start_cursor, 42);
    assert_eq!(page.end_cursor, 46);
}

#[test]
fn retained_page_returns_the_complete_buffer() {
    let mut output = OutputBuffer::default();
    output.push("prefix 你好".as_bytes());
    output.push(b" suffix");
    let page = output.retained_page();
    assert_eq!(page.data, "prefix 你好 suffix");
    assert_eq!(page.start_cursor, 0);
    assert_eq!(page.end_cursor, "prefix 你好 suffix".len() as u64);
    assert!(!page.has_more);
    assert!(!page.cursor_expired);
}

#[test]
fn retained_tail_page_is_bounded_and_keeps_an_incomplete_suffix_for_later() {
    let mut output = OutputBuffer::default();
    output.push(b"prefix-");
    let encoded = "你".as_bytes();
    output.push(&encoded[..2]);

    let tail = output.retained_tail_page(4);

    assert_eq!(tail.data, "x-");
    assert_eq!(tail.start_cursor, 5);
    assert_eq!(tail.end_cursor, 7);
    assert!(tail.has_more);

    output.push(&encoded[2..]);
    let remainder = output.page_since(tail.end_cursor, 64);
    assert_eq!(remainder.data, "你");
    assert_eq!(remainder.end_cursor, 10);
}

#[test]
fn trimming_retained_output_keeps_the_latest_bytes_and_absolute_cursor() {
    let mut output = OutputBuffer::with_capacity_at(1024, 40);
    output.push(b"0123456789");

    output.trim_to(4);
    let retained = output.retained_page();

    assert_eq!(retained.data, "6789");
    assert_eq!(retained.start_cursor, 46);
    assert_eq!(retained.end_cursor, 50);
    assert_eq!(output.retained_bytes(), 4);
}

#[test]
fn history_since_extends_a_page_to_the_next_utf8_boundary() {
    let mut output = OutputBuffer::default();
    output.push("ab你cd".as_bytes());
    let page = output.page_since(0, 3);
    assert_eq!(page.data, "ab你");
    assert_eq!(page.start_cursor, 0);
    assert_eq!(page.end_cursor, 5);
    assert!(page.has_more);
}

#[test]
fn forward_pages_wait_for_an_incomplete_utf8_suffix() {
    let mut output = OutputBuffer::default();
    let encoded = "你".as_bytes();
    output.push(&encoded[..2]);
    let partial = output.retained_page();
    assert!(partial.data.is_empty());
    assert_eq!(partial.end_cursor, 0);
    assert!(partial.has_more);
    output.push(&encoded[2..]);
    let completed = output.page_since(partial.end_cursor, 64);
    assert_eq!(completed.data, "你");
    assert_eq!(completed.end_cursor, 3);
    assert!(!completed.has_more);
}

#[test]
fn history_since_only_returns_new_output() {
    let mut output = OutputBuffer::default();
    output.push(b"hello");
    let first = output.page_since(u64::MAX, 1024);
    assert!(first.data.is_empty());
    assert_eq!(first.end_cursor, 5);
    output.push(b" world");
    let next = output.page_since(first.end_cursor, 1024);
    assert_eq!(next.data, " world");
    assert_eq!(next.start_cursor, 5);
    assert_eq!(next.end_cursor, 11);
    assert!(!next.has_more);
}

#[test]
fn bounded_forward_pages_ignore_output_after_the_snapshot_end() {
    let mut output = OutputBuffer::default();
    output.push(b"snapshot");
    let snapshot_end = output.retained_page().end_cursor;
    output.push(b"-live-output");

    let first = output.page_since_until(0, snapshot_end, 4);
    assert_eq!(first.data, "snap");
    assert!(first.has_more);

    let second = output.page_since_until(first.end_cursor, snapshot_end, 64);
    assert_eq!(second.data, "shot");
    assert_eq!(second.end_cursor, snapshot_end);
    assert!(!second.has_more);
}

#[test]
fn bounded_forward_pages_stop_before_an_incomplete_utf8_suffix() {
    let mut output = OutputBuffer::default();
    output.push(b"ready ");
    let encoded = "你".as_bytes();
    output.push(&encoded[..2]);
    let snapshot_end = output.retained_page().end_cursor;
    assert_eq!(snapshot_end, 6);

    let page = output.page_since_until(0, snapshot_end, 64);
    assert_eq!(page.data, "ready ");
    assert_eq!(page.end_cursor, snapshot_end);
    assert!(!page.has_more);

    output.push(&encoded[2..]);
    assert_eq!(output.page_since(snapshot_end, 64).data, "你");
}

#[test]
fn tracks_bracketed_paste_across_output_chunks() {
    let mut output = OutputBuffer::default();
    output.push(b"\x1b[?20");
    output.push(b"04hready");
    assert!(output.bracketed_paste_enabled());
    output.push(b"\x1b[?2004l");
    assert!(!output.bracketed_paste_enabled());
}

#[test]
fn partial_utf8_pages_preserve_cursor_byte_length() {
    let mut output = OutputBuffer::default();
    output.push("你".as_bytes());
    let page = output.page(None, 2);
    assert_eq!(page.data, "??");
    assert_eq!(page.data.len() as u64, page.end_cursor - page.start_cursor);
}

#[test]
fn raw_pages_preserve_non_utf8_bytes_and_exact_cursors() {
    let mut output = OutputBuffer::with_capacity_at(6, 40);
    output.push(&[b'a', 0xff, b'b', 0xfe, b'c', b'd', b'e']);

    let retained = output.raw_retained_since(Some(40));
    assert_eq!(retained.data, [0xff, b'b', 0xfe, b'c', b'd', b'e']);
    assert_eq!(retained.start_cursor, 41);
    assert_eq!(retained.end_cursor, 47);
    assert!(retained.cursor_expired);

    let first = output.raw_page_since(41, 3);
    assert_eq!(first.data, [0xff, b'b', 0xfe]);
    assert_eq!(first.end_cursor, 44);
}
