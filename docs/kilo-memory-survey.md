# Kilo/OpenCode lane memory on claw

The largest removable baseline cost is the native attach TUI. An opt-in Actor
environment setting, `CCCC_OPENCODE_HEADLESS=1`, delivers through ACP and starts
the TUI only while terminal connections are open. Kilo's official npm launcher
also retains about 50 MB of Node RSS per process; this opt-in mode reuses its native
cache to remove that overhead from the server and any open viewer. With the flag
unset, the original npm launch and terminal-delivery paths remain in use.

All measurements below use private claw processes and homes. No production
deployment, install or daemon restart occurred on MACDAD or firestar. These are
Linux baseline measurements, not a long-term leak or macOS validation.

## Fixture and units

- CCCC parent: `c36a4e4c`; branch: `sol-hi-b/kilo-memory`.
- Pinned daemon/CLI binary SHA-256: parent
  `6c130d3cf0fadd5e681451878ded4be88f6efbcd352cf4a2ffcc7b32853832bf`;
  candidate at `303726ab`
  `2fbd0247d257fc5f58368b9751a3d0c8bf8d7a771fc5021af9fc20be9893d4cf`;
  default-equivalence follow-up
  `01ff63f8cdf9d3e1654ffb8749875cf1f3a409b0d42dc5a5f01b54f12008467b`.
- Kilo CLI: 7.8.8, embedded Bun 1.4.2. OpenCode: 1.19.0, embedded Bun 1.3.14.
- Native executable SHA-256: Kilo
  `d734bdfd765781457034fa0b8fd5bd28fa4d21472eab0ac030a1db7f571bfded`;
  OpenCode `135ac3f86c43f2a22328a3554fec5f0bc57d71576077902661bc38dac34871fd`.
- Checkout: obs-studio `a0e6ea3df8764c7cd995fb71d54721d1d4558078`, 5,526
  tracked files, private clone under `/home/ghrunner/kilo-memory-lab/repo`.
- Each native probe creates an isolated XDG configuration/data/state/cache home,
  initializes ACP, creates a session, and completes one synthetic turn against a
  loopback OpenAI-compatible fixture. No real provider credentials or paid inference.
- RSS is decimal MB, summed over each server/viewer process tree. Each result is
  the median of the last ten one-second samples in a 30-second settling window.
  RSS counts shared mappings in each process; it is not PSS.
- Raw samples: `/home/ghrunner/kilo-memory-lab/<case>/result.json` on claw.

## Native runtime probes

| Change | Kilo RSS (MB) | OpenCode RSS (MB) | Interpretation |
| --- | ---: | ---: | --- |
| Default server plus viewer | 1,255 / 1,213 through npm | 1,052 native | Includes both runtimes |
| Viewer absent | 702 through npm | 680 / 641 native | Removing the TUI is the main structural gain |
| Native server, viewer absent | 654 / 659 | Already native | Kilo removes one resident Node process |
| Native server plus native viewer | 1,122 | Already native | Kilo removes both resident Node processes |
| Native server with watcher disabled | 535 / 539 | 719 | Repeatable Kilo saving around 119 MB; no OpenCode saving established |
| Native server with `BUN_OPTIONS=--smol` | 631 / 653 | 646 / 664 | Repeats overlap controls; no dependable saving established |
| Native server with `snapshot:false` | 650 | 808 | No idle RSS saving established |
| Native server with `lsp:false` | 656 | 706 | No idle RSS saving established |

Two Kilo wrapper/viewer runs each retained about 101 MB in the two Node shims
alone. The difference between total native and wrapper runs ranges from 91 to
133 MB because the Bun heaps also vary. A closed viewer removes its entire attach
process tree, not just Node. OpenCode 1.19.0's official npm package installs a
native ELF as `bin/opencode.exe`; there is no resident npm Node shim to remove in
this version.

A Kilo native read of `libobs/obs.c` completed through the real `read` tool and
settled at 665 MB. No LSP process or downloaded clangd artifact appeared. This
does not establish the cost or correctness of disabling an active language server.
The snapshot/LSP rows describe the idle fixture only.

## Server settings and risks

Kilo's measured watcher setting is
`KILO_EXPERIMENTAL_DISABLE_FILEWATCHER=true`. The compiled 7.8.8 binary contains
this flag, and the two separate measurements support the saving above. The
[upstream watcher implementation](https://github.com/Kilo-Org/kilocode/blob/main/packages/core/src/filesystem/watcher.ts)
returns an empty watcher service when it is enabled. Native watcher events, including Git HEAD changes, disappear; keep it an explicit Actor configuration choice. CCCC does not
enable it automatically.

OpenCode's corresponding `OPENCODE_EXPERIMENTAL_DISABLE_FILEWATCHER=true` did
not save memory in this fixture. `snapshot:false` removes snapshot rollback;
`lsp:false` removes language-server diagnostics/navigation. Neither showed a
useful idle memory reduction here, so this branch does not change those defaults.
See the [OpenCode configuration documentation](https://opencode.ai/docs/config/)
and [Kilo CLI documentation](https://kilo.ai/docs/code-with-ai/platforms/cli).

Both compiled binaries accepted `BUN_OPTIONS=--smol`: executing their embedded
Bun with `BUN_BE_BUN=1` printed `--smol` in `process.execArgv`. This verifies the
setting reaches these specific executables; it does not prove a useful RSS gain.
[Bun documents compiled-executable options](https://bun.sh/docs/bundler/executables)
and the [extra garbage collection tradeoff of `--smol`](https://bun.sh/docs/runtime).
The fixture scopes `BUN_OPTIONS` to its native process environment; it is never
exported globally. [Bun issue #39377](https://github.com/oven-sh/bun/issues/39377)
reports recursive `bunx` subprocess creation when that variable is inherited,
including Bun 1.3.14. This branch does not add `BUN_OPTIONS` to any launch defaults.
CPU comparisons use 20 synthetic turns and Linux process CPU counters, including
runtime GC work; they do not isolate time spent in GC.

| 20-turn arm | Turn CPU seconds | Elapsed seconds | Settled CPU (% one core) | Settled RSS (MB) |
| --- | ---: | ---: | ---: | ---: |
| Kilo control | 9.70 | 12.955 | 1.547 | 680.436 |
| Kilo `--smol` | 9.83 | 13.188 | 1.534 | 666.554 |
| OpenCode control | 28.93 | 59.960 | 58.314 | 792.035 |
| OpenCode `--smol` | 25.14 | 47.627 | 47.663 | 960.948 |

These single comparisons do not establish an active-workload benefit or isolated
GC cost. OpenCode continued consuming about half a core during the last nine
seconds of the 30-second settling window in both arms. Its lower CPU reading with
`--smol` accompanies substantially higher RSS, so there is no basis for enabling
that setting here. The high OpenCode post-turn CPU is a separate profiling lead;
this experiment does not identify its cause. The separate P3 follow-up is card T502.

## Delivery and viewer contract

Exactly-once on clean paths; at-most-once plus a visible error on ambiguous admission or crash.

Source event IDs correlate ACP submissions. The original delivery batch stays
fixed while queued; ACP refuses admission during an active turn. Accepted receipts
retire only after the existing delivery ledger record is durable. The pending
receipt cache holds at most 256 entries, with retained diagnostics limited to about
4 KB each. It is bounded by queueing, never eviction. Explicit protocol rejection
before admission is retryable; unknown provider errors or lost admission replies
are uncertain and never replay automatically. The surfaced error includes the
source event ID so a human can resend deliberately. An uncertain source settles
as ambiguous without blocking all later messages indefinitely.

Terminal connections hold leases on one viewer. Closing the last lease stops
only the TUI; the ACP provider and native session remain alive. Stale leases cannot
stop a replacement session. The current terminal writer's Ctrl-C maps to ACP
cancel; passive or superseded connections cannot cancel. There is no silent
fallback to typing into a newly spawned TUI after ACP delivery fails.

The native Kilo launch path recognizes only the official npm installation layout,
uses its postinstall-selected `.kilo` cache or absolute `KILO_BIN_PATH`, and preserves
tree-sitter resources. Uncached packages and relative overrides retain the official
launcher's existing selection semantics. Native cache validation on macOS/Windows
and production behavior remain platform validation boundaries.

## Reproduction

The scripts in `tools/` require Python's standard library and installed native
Kilo/OpenCode executables. Each case creates its own home and cleans up only its
own process groups. The daemon proof requires a freshly built private `cccc`
binary and never uses an existing daemon home.

```bash
python3 tools/kilo-memory-probe.py \
  --root /home/ghrunner/kilo-memory-lab \
  --repo /home/ghrunner/kilo-memory-lab/repo \
  --runtime kilo --executable /path/to/native/.kilo --case new-control

python3 tools/kilo-headless-daemon-proof.py \
  --root /home/ghrunner/kilo-memory-lab \
  --repo /home/ghrunner/kilo-memory-lab/repo \
  --binary /path/to/private/cccc \
  --runtime kilo --executable /path/to/npm/bin/kilo \
  --case new-daemon-proof --expect-headless
```

Use `--viewer`, `--no-watcher`, `--smol`, or `--config '{"snapshot":false}'` on
the native probe for the corresponding cases. Add `--turns 20` for the CPU
comparison. Case names must be unused.

## Private-daemon measurements

The preserved parent and candidate each start one Actor in their own CCCC home,
with the same large checkout and native runtime version. Actor child-process RSS
includes the managed server, MCP process, viewer when present, and small ownership
watchdog processes; it excludes the CCCC daemon itself.

| Runtime / case | Startup (MB) | Viewer closed (MB) | Viewer open (MB) | Closed again (MB) |
| --- | ---: | ---: | ---: | ---: |
| Kilo parent, `daemon-kilo-baseline-rss` | 1,212.305 | — | TUI starts immediately | — |
| Kilo at `303726ab`, `daemon-kilo-final` | 670.802 | 666.185 | 1,175.003 | 725.947 |
| Kilo follow-up, `daemon-kilo-default-fix` | 671.629 | 672.493 | 1,161.417 | 737.202 |
| OpenCode parent, `daemon-opencode-baseline-rss` | 1,119.411 | — | TUI starts immediately | — |
| OpenCode at `303726ab`, `daemon-opencode-final` | 525.054 | 606.990 | 1,009.287 | 641.622 |
| OpenCode follow-up, `daemon-opencode-default-fix` | 618.288 | 677.790 | 1,016.185 | 708.608 |

Kilo's matched `303726ab` startup comparison saves 541.503 MB (44.67%) per lane.
Closing its last viewer frees the attach process and reduces lane RSS by 449.056 MB
relative to the open measurement. The provider retains about 59.8 MB above its
pre-viewer closed heap. Earlier candidates measured startup RSS of 638/687 MB and
closed-again RSS of 732/727 MB; this variation is why the final pinned run is
reported explicitly rather than selecting the smallest result.

The attach processes themselves measured 420.516 MB for Kilo and 304.894 MB
for OpenCode in the final open windows. The larger whole-lane closing deltas also
include provider heap movement; they should not be attributed entirely to the TUI.

OpenCode's matched `303726ab` startup comparison saves 594.357 MB (53.10%) per lane. Its
viewer close frees 367.665 MB relative to the open measurement; after reopening
and closing, the server also retains additional heap. These are process-tree RSS
observations, not a promise that every provider heap returns to its initial size.
The `*-final` cases use the pinned `303726ab` binary listed above; the earlier Kilo
candidates preceded the diagnostic-cap and resource-path hardening. The startup
before/after savings combine headless delivery and native launch; do not add the
individual Node savings to that combined total.

The first real Kilo proof submitted three messages, held the first model request
while the second waited, observed them in order, and recorded each accepted source
ID exactly once. A clean restart restored the same native session ID before the
third delivery. Two terminal connections shared one viewer, and only the last
close freed it. The extended Kilo proof passed managed working/idle state, native cancel, passive
viewer refusal, four exactly-once accepted receipts, and the same-session restart. OpenCode passed
the same extended checks on the pinned final binary. The final Kilo run passed
them as well; each final run recorded four source IDs exactly once in order.

## Default-path equivalence follow-up

Seat review required flag-unset equivalence. Three regressions failed at
`303726ab`: an installed native cache changed default Kilo argv; pre-admission
rejection changed legacy Voice Analyst retry fencing; and closed terminal output
errors were globally normalized. The follow-up gates native launch/resources,
managed cancellation, and closed-socket normalization under the opt-in, and keeps
legacy Voice Analyst rejection semantics. The default launcher test also checks
flag `0`, executable-name restrictions, and an unchanged resource environment.
The closed-socket regression exercises an actual terminal attachment, rather than
only checking a helper's condition. The pre-existing detach fixture accepts EOF
or the original closed-write error after it closes its own socket.

The new binary repeated the complete native Kilo and OpenCode proofs successfully.
Kilo startup was 671.629 MB, a 540.676 MB (44.60%) reduction against the parent;
its viewer measured 424.010 MB, and closing the last viewer reduced lane RSS
by 424.215 MB. OpenCode's
startup RSS was 618.288 MB, a 501.123 MB (44.77%) reduction against the parent.
The viewer itself measured 308.232 MB; closing its last connection reduced lane
RSS by 307.577 MB. This repeat is higher than the original 525.054 MB startup
result. Both measurements are retained rather than attributing ordinary provider
heap variation to a default-path guard change.

## Validation

Native launcher: two regressions fail on the exact parent when a selected native
cache is available. Headless startup: the regression fails on the parent because
it starts a TUI. The follow-up source passed 597 daemon tests and 141 runtime
tests, including
the three flag-unset regressions and external native-override resource-path
regression. The three flag-unset regressions were red at `303726ab` and green
with these fixes. After clippy's equivalent Boolean guard cleanup, all nine
launcher tests and all three flag-unset tests passed again. Clippy passed for
both crates
with all targets/features and `-D warnings`; workspace formatting is clean. The
final CLI build passed. Both real-daemon proofs passed on the original candidate
and repeated on the pinned default-equivalence follow-up binary:
ordered queued admission, durable receipts, working/idle detection, shared viewer
last-close cleanup, same-session restart, native cancel, and passive refusal.

The preserved parent daemon measured 1,212 MB of actor child-process RSS after
idle startup, including the provider, TUI, npm shims and MCP process. Its own daemon
RSS is excluded. A separate parent PTY delivery attempt wrote an accepted ledger
receipt but did not reach the local model before the fixture's deadline. This
attempt is not delivery correctness evidence. The final candidates passed the
ACP delivery, durable receipt and same-session restart proofs independently.
