DRAFT reply to ChesterRa/cccc issue #126 (closed) — for owner decision before posting.
Context: maintainer retained the TUI delivery path; invited "a focused follow-up if you find a simpler way to preserve native configuration behavior and reliable delivery without substantial additional machinery."

=== DRAFT REPLY ===

Hi @ChesterRa — following up on your invitation, with the simpler shape and the measurements you asked about. We've been running this in our production fleet since 2026-10-10.

**The shape: one delivery path, viewer as pure display.** Rather than the hybrid (backend when no TUI, PTY when present — the two-path problem you flagged), we moved delivery entirely onto the managed ACP session and made the native TUI a lazy viewer that starts when someone opens the actor's terminal and exits when the last connection closes. There is exactly one delivery path, so there are no path-transition cases to validate; flag unset (`CCCC_OPENCODE_HEADLESS`) keeps today's PTY delivery byte-for-byte.

On your specific concerns:

- **Configuration consistency / the lost `high` reasoning variant:** we reproduced that gap too. The fix lives in the ACP/SSE bridge: when a user message submitted through any interface carries a model selection, the stream observer calls `set_config_option(session_id, "model", …)` on the existing backend session, so a TUI-submitted selection reaches ACP before the next submission (`synchronized_model` in `opencode/stream.rs`). Selection stays the provider's own per-session state — we built no settings panel and duplicated no configuration logic.
- **Delivery semantics:** submissions go through the managed ACP session with admission fencing — explicit pre-admission rejection is retryable, uncertain admission is fenced against replay and surfaces the source event ID. Exactly-once on clean paths; at-most-once plus a visible error on ambiguous admission or crash. Receipts retire only after durable acceptance.
- **Busy turns:** submissions queue behind an active turn instead of being rejected — the adapter constraint you identified, handled at queue level.
- **Cancellation:** the terminal writer's Ctrl-C maps to ACP cancel; turn correlation is unchanged since it is the same provider session.

**Measurements** (claw, Skylake 8 vCPU / 15 GB / Debian 13, same checkout, pinned binaries): Kilo 7.8.8 parent 1,212 MB → 672 MB (−44.6%); OpenCode 1.19.0 parent 1,119 MB → 618 MB (−44.8%). The removed attach viewers measured ~424/308 MB each — consistent with the 243–285 MiB PSS you measured on 1.18.33.

**Validation:** 597 daemon tests + 141 runtime tests green on claw; `clippy --all-targets -- -D warnings` and `fmt` clean. Three default-path regressions caught by the first candidate each failed there and pass with the follow-up — the flag stays opt-in precisely so the stock path is untouched.

If the tradeoff looks right to you now, we can prepare a focused PR — opt-in flag, ACP delivery, lazy viewer — rather than a large one. Happy to answer questions on any of the delivery semantics first.

=== END DRAFT ===
Notes for reviewer: every claim above is backed by sol-hi-b/kilo-memory branch code (set_config_option sync, queue-behind-active-turn, admission fencing) or the measurements table in our cccc#7. No new claims.
