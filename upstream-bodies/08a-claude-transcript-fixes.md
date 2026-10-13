BRANCH: chriscoveries/cccc up/claude-transcript-fixes  (base: upstream/main d1ba8424)
SUPERSEDES: 08-claude-continuity.md (split per seat ruling)
DUP-CHECK: no upstream issue/PR covers these three transcript gaps. Narrow PR; no overlap with our merged #133.

=== UPSTREAM ISSUE ===
Title: three Claude transcript-handling gaps: unreadable mismatch diagnostics, interruption mis-correlation, and a publish-before-create race

Three small managed-Claude issues observed in production: (a) when Agent View publishes a transcript for a different session the log cannot say which identity arrived; (b) an interrupt can be correlated to the wrong assistant turn; (c) a transcript path published before its file exists disconnects the session immediately instead of using the bounded missing-file grace.

=== PR BODY ===
Hi @ChesterRa — three small, independent transcript-handling fixes for managed Claude actors, each from a real production incident.

**(a) Identify mismatched transcript diagnostics.** When Agent View publishes a transcript under a different session identity, the diagnostic now names the expected session UUID, the observed transcript filename, and the managed job identifier — quoted fields, no directory paths. The existing InvalidData rejection and session-ownership checks are unchanged; the regression confirms no foreign transcript is adopted.

**(b) Correlate interruptions by active assistant parent.** Interruption handling previously guessed at the latest turn; it now correlates to the active assistant parent, so an interrupt can't be attached to the wrong turn.

**(c) Tolerate delayed initial transcript publication.** Agent View may publish its exact-session transcript path before creating the file; fresh initialization and polling previously bypassed the bounded missing-file grace until a path had been adopted, causing an immediate disconnect. The existing ten-second continuity grace now applies before the first transcript is available too — identity validation and the first-input offset are preserved, and permanent absence still fails. Tests cover delayed creation, delivery without replay, and permanent absence.

## How we built it

Base commit d1ba8424. Three commits, one per gap (+~210 lines incl. tests), confined to `claude.rs`, `claude/transcript.rs`, and `claude/transcript_path.rs`. All are narrow behavioral fixes — no interface or format changes.

## How we tested it

- `cargo test -p cccc-pair-daemon --lib transcript` : 55 passed; `--lib claude` : 91 passed, 0 failed — including `first_transcript_publication_wait_is_bounded` and the relocation/mismatch regressions.
- `cargo fmt`/`cargo clippy --all-targets -- -D warnings` on cccc-pair-daemon: clean.
- Test env: Skylake 8 vCPU, 15 GB, Debian 13. In production on firestar (build 0990769c) since 2026-10-10.
