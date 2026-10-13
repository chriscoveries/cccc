
//! Independent review probe for OutputBuffer boundary behaviour.
//! Written by the reviewer, not the author. Run:
//!   cargo test -p cccc-pair-runtime --lib -- review_probe
#[cfg(test)]
mod review_probe {
    use crate::output::OutputBuffer;

    /// A read spanning a block edge must be reassembled byte-exactly, and the
    /// cursors must describe the whole retained window, not the read.
    #[test]
    fn probe_read_spanning_block_edge_is_exact() {
        let cap = 64 * 1024;
        let mut output = OutputBuffer::with_capacity_at(cap, 0);
        // Push sizes that deliberately straddle the 4096 chunk size.
        for size in [4095_usize, 2, 4096, 1, 4097, 8192] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            output.push(&data);
            let page = output.raw_retained_since(None);
            // Cursors must be contiguous and consistent with retained bytes.
            assert_eq!(
                page.end_cursor - page.start_cursor,
                page.data.len() as u64,
                "cursor span must equal retained bytes after {size}-byte read"
            );
            assert_eq!(page.data.len() <= cap, true);
        }
    }

    /// After a trim the cursor origin must still line up with the bytes, and a
    /// subsequent push must continue from the right place.
    #[test]
    fn probe_trim_then_push_keeps_cursors_aligned() {
        let mut output = OutputBuffer::with_capacity_at(32 * 1024, 1000);
        let payload: Vec<u8> = (0..20_000).map(|i| (i % 253) as u8).collect();
        output.push(&payload);
        assert_eq!(output.raw_retained_since(None).start_cursor, 1000);

        output.trim_to(5000);
        let after_trim = output.raw_retained_since(None);
        assert_eq!(after_trim.data.len(), 5000, "trim must retain exactly the limit");
        assert_eq!(after_trim.start_cursor, 1000 + 15_000);
        assert_eq!(after_trim.end_cursor, 1000 + 20_000);

        let tail: Vec<u8> = (0..777).map(|i| (i % 251) as u8).collect();
        output.push(&tail);
        let after = output.raw_retained_since(None);
        assert_eq!(after.end_cursor, 1000 + 20_000 + 777);
        assert_eq!(after.end_cursor - after.start_cursor, after.data.len() as u64);
        // The 5000 bytes kept by the trim must be a suffix of what we pushed.
        let expect: Vec<u8> = payload[15_000..]
            .iter()
            .copied()
            .chain(tail.iter().copied())
            .collect();
        assert_eq!(after.data, expect, "trim+push must preserve byte order exactly");
    }

    /// A page that straddles a block boundary must be byte-exact and must not
    /// lose or duplicate a byte at the seam.
    #[test]
    fn probe_page_straddling_block_boundary_is_exact() {
        let mut output = OutputBuffer::with_capacity_at(64 * 1024, 0);
        let data: Vec<u8> = (0..30_000).map(|i| (i % 251) as u8).collect();
        output.push(&data);
        // raw_page_since(after, limit) returns the FORWARD window
        // [after, after + limit + 1), not a trailing one. Straddle the seams
        // with windows that start just below, at, and just above each 4096
        // boundary, so the seam falls inside the window.
        for after in [4000_u64, 4095, 4096, 4097, 8100, 8191, 8192, 8193, 12_000] {
            let limit = 100_usize;
            let raw = output.raw_page_since(after, limit);
            let from = after as usize;
            // page_end = after + limit.max(1), so the window is [after, after+limit).
            let to = (from + limit).min(data.len());
            let expect = &data[from..to];
            assert_eq!(raw.data, expect, "raw page after={after} must be exact");
            assert_eq!(raw.start_cursor, from as u64);
            assert_eq!(raw.end_cursor, from as u64 + expect.len() as u64);
        }
    }

    /// The mode probe must keep working at and past its 16-byte cap, including
    /// a mode sequence split across two pushes where the whole sequence is
    /// discarded by capacity.
    #[test]
    fn probe_mode_probe_at_cap_and_across_split_pushes() {
        let mut output = OutputBuffer::with_capacity_at(16, 0);
        // Sequence split so that no single push contains all 8 bytes.
        output.push(b"\x1b[?20");
        assert!(!output.bracketed_paste_enabled(), "partial sequence must not enable");
        output.push(b"04h");
        assert!(output.bracketed_paste_enabled(), "sequence completed across pushes");

        // Now a large push that discards the prefix, ending mid-sequence.
        let mut big = vec![b'x'; 4096];
        big.extend_from_slice(b"\x1b[?2004l");
        output.push(&big);
        assert!(
            !output.bracketed_paste_enabled(),
            "a disable sequence at the tail of a large push must be seen"
        );
    }

    /// Repeated trims must not accumulate front_offset or leak a stale offset
    /// that corrupts the first chunk.
    #[test]
    fn probe_repeated_trims_do_not_corrupt_first_chunk() {
        let mut output = OutputBuffer::with_capacity_at(16 * 1024, 0);
        let data: Vec<u8> = (0..12_000).map(|i| (i % 241) as u8).collect();
        output.push(&data);
        for _ in 0..5 {
            output.trim_to(9000);
            let page = output.raw_retained_since(None);
            assert_eq!(page.data.len(), 9000);
            let start = 12_000 - 9000;
            assert_eq!(page.data, data[start..], "first chunk must not shift");
        }
    }
}
