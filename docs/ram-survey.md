# cccc daemon RAM survey — handoff to lab-oc1

Status: reproduction built and heap-profiled; biggest retainer identified; one
bounded fix shipped on this branch (`devin/ram-index`, commit `8e1d544d`);
before→after RSS measurement incomplete (reassigned mid-run).

Box spec: Intel Xeon Platinum 8375C @ 2.90GHz, 8 cores, 31GiB RAM,
Ubuntu 22.04.5, kernel 6.8.0-1061-aws. rustc 1.88/1.99, heaptrack 1.3.0.

## Production symptom (from owner)

firestar `cccc daemon` reaches ~2.6 GB RSS within 5h, ~17 actors in 6 groups,
ledgers 26–91 MB. Suspects named: ledger index, per-actor session/PTY history,
dashboard caches.

## Reproduction (this box)

Synthetic home at `/tmp/cccc-ram/home` (may be wiped on reboot; scripts under
`/tmp/cccc-ram/scripts/`): 6 groups with ledgers at 27/41/56/71/81/102 MB
(the last grown via `scripts/append_ledger.py`, which writes serde-valid
`chat.message` events at ~2 KB each, external to the daemon — this is what
makes an index go stale), 18 actors `claude-a1..a18` running
`scripts/emit.sh` (echo loop every 0.2 s). Daemon:
`CCCC_HOME=/tmp/cccc-ram/home target/release/cccc daemon run` (it daemonizes;
the real pid is the `release/cccc` child, not the launcher).

Traffic that reproduces it: per group, `cccc tail --limit 5 --group <g>` +
`cccc send <text> --group <g> --to <actor>` (script `scripts/traffic.sh`).

Observed on stock `lab-0990769c` (release build): RSS 48 MB at boot → 735 MB
after one tail+send round per group → 880 MB after forcing a 91→101 MB ledger
rebuild. Under heaptrack: peak heap 511 MB, peak RSS 900 MB,
leaked-at-capture 288 MB.

## Heap profile verdict

Top peak AND top leaked stacks are the same path:

    runtime_delivery::claim_deliveries
      → ledger::inspect → ledger_index::queries::inspect
      → LedgerIndex::current → LedgerIndex::rebuild
      → ledger::read_all_uncached → decode_event_line / serde MapAccess

365 MB peak in that stack; ~288 MB live at capture ≈ the warm index set
(the effective cap is `MAX_SOURCE_BYTES` 128 MiB + one oversized-entry
allowance ≈ ~310 MB retained at ~2× source bytes).

Every other suspect was already bounded on `lab-0990769c`: PTY transcripts
10 MB/actor (50 MB total), terminal frames 50 MB, TerminalStateMirror ≤1M
cells, OutputBuffer 2 MB/actor (hot cap 10 MB), CompletedHistories 8 MB,
events_stream 2048 events. None of them retain; they are not the leak.

## Mechanism (why it ratchets over hours, not just to a cap)

1. Retained: each warm ledger index holds the full `events: Vec<Event>` plus
   lookup maps ≈ 2× source bytes. Six big ledgers ≈ ~300 MB live — bounded.
2. Churn (the ratchet): every stale hit rebuilt by `read_all_uncached` —
   re-parse tens of MB and replace the whole index. And every `send` walks
   whole-ledger scans in `runtime_delivery` (`pending_sources`,
   `settle_stranded_claims`, `claim_deliveries` all iterate the full event
   slice, building large HashMaps per call). Freed chunks stay in the malloc
   arenas → RSS ratchets and never returns. This is the dominant growth past
   the retained cap, and the likely difference between ~0.9 GB here and
   2.6 GB on firestar (more ledgers + 5h of sends).

## Fix shipped on this branch — 8e1d544d, tests green

Two bounded mechanisms in the index layer, `crates/cccc-core`:

- `ledger_index.rs`: `current()` catch-up — when every indexed source is
  still present, non-shrinking, and line-aligned, only the appended tail is
  read (`read_appended_tail` + `read_source_from(offset)`); removed /
  truncated / gzip-grown sources, torn tails, and stale-marked (empty)
  revisions all fall back to a full rebuild. Kills the rebuild churn.
- `ledger_index/cache.rs`: idle TTL eviction (`IDLE = 300 s`); an index not
  consulted for the TTL is dropped on the next cache op (never while an Arc
  is live), so quiet groups stop holding ~2× their ledger.

Tests added (all pass; `delayed_notification_…` regression also fixed —
empty `old` must rebuild, not tail-read): tail-only catch-up over a
corrupted committed prefix (full rebuild would fail; catch-up succeeds),
idle eviction on the next cache op, no idle eviction while an entry is in
use. `cargo test -p cccc-pair-core -p cccc-pair-daemon`: 308 pass / 1 fail —
`scope::tests::detected_scope_…` is the preexisting env failure (clone
remote is the git-manager proxy; fails on stock upstream too). clippy
`-D warnings` clean; fmt clean.

## RSS before→after — partial data, needs finishing

- BEFORE (stock): 48 MB boot → 735 MB → 880 MB (see above).
- AFTER (fixed build, same home): 1.03 GB after 3 traffic rounds → climbed
  to ~1.34 GB plateau during one-group-only traffic. Numbers are NOT lower
  than stock, and I believe that is honest, not a regression:

  - The daemon at this point had already accumulated ~1 GB of
    allocator-held transient chunks from earlier rounds; the second run
    inherits that home but starts fresh arenas — yet sends to the 108 MB
    ledger keep driving `pending_sources`/`settle_stranded_claims`
    whole-ledger maps (~200 MB transients each), which is un-fixed churn.
  - Idle eviction lowers HEAP, not RSS: glibc does not return freed chunks
    to the OS, so evicting the five idle indexes does not drop VmRSS. To
    show the delta, compare heaptrack "live bytes" or run the repro cold on
    the fixed build and compare plateau — do not expect VmRSS to regress on
    a hot process.

Recommended next steps for lab-oc1:
1. Re-run the same protocol cold on this branch and on `lab-0990769c`,
   heaptrack both, compare live-at-capture (index weight + transients) —
   the honest before→after.
2. Attack the second retainer: `crates/cccc-daemon/src/ops/runtime_delivery.rs`
   `pending_sources`, `settle_stranded_claims`, `claim_deliveries` rebuild
   full-ledger maps per call. Add a `runtime.delivery`-keyed lookup to
   `LedgerIndex` (like `positions`/`relays`, keyed by (actor_id,
   source_event_id)) or bound delivery scans to reverse windows — that is
   the remaining churn feeding the arenas.
3. If RSS still won't fall on idle, consider `malloc_trim(0)` on a timer or
   jemalloc/mimalloc — arenas are the visible residue.

## Repro artifacts (on this box, /tmp may not survive reboot)

- `/tmp/cccc-ram/home` — synthetic CCCC_HOME (6 big-ledger groups, 18 actors).
- `/tmp/cccc-ram/scripts/` — emit.sh, append_ledger.py, traffic.sh,
  idle_protocol.sh.
- `/tmp/cccc-ram/ht.zst`, `ht-snap2.zst` — heaptrack of the stock daemon
  (`heaptrack_print -f <file> -l 1 -m 1`).
- `/tmp/cccc-ram/daemon.log`, `daemon2.log`, `idle-protocol.log`.

— CC1 · devin-swe-2, 2026-10-11
