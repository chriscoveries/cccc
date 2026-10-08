# Managed Claude jobs across daemon restarts

The global `CCCC_HOME/settings.yaml` policy is opt-in:

```yaml
runtime:
  claude_daemon_exit: detach # stop (default) or detach
```

Set it before starting the daemon and managed Claude actors. The default `stop`
policy retains existing stop, submission, launcher environment, and shutdown
settlement behavior. `detach` closes delivery workers, native viewers, and
transcript observers without stopping the Claude Agent View provider job.
Other providers still stop. Forced-exit provider requests honor this policy too.

Detachment requires runtime resume to be enabled (`CCCC_RUNTIME_RESUME` must
not be `0`, `false`, `no`, or `off`). Windows detach is rejected. Invalid policy
values are rejected when settings load.

## Recovery and explicit lifecycle actions

Startup re-adopts an eligible actor's surviving job through its saved session
ID, without creating a fresh conversation. Busy jobs remain running and retry
with exponential backoff from 250 ms to at most four seconds between probes.
Backoff releases the group dispatch lock. Settlement polling runs outside that
lock; ownership is revalidated under the lock before attaching. Configuration
validation and attachment still run under the lock. A stopped group, disabled
actor, replaced actor generation, or changed saved session cancels recovery.

A separate, atomic `<actor>.owner.json` record beside the runtime session
receipt preserves actor generation, original canonical workspace, provider
configuration directory, and session ownership. A pending sibling remains
eligible across another restart even if stopping another actor clears the
registration-derived `group.running` aggregate. A historical receipt without
running intent is only recovered when its exact live job can be validated.

Actor Stop, Restart, New Session, Group Stop, and the stopped-state path address
registered jobs or validated saved ownership. Teardown uses the original
workspace/configuration; New Session launches its replacement with current
configuration. Empty/invalid saved IDs never become provider stop targets.
Legacy receipts without ownership metadata require the original launch identity
to resolve their provider configuration. If that identity or original workspace
cannot be verified, teardown fails visibly and leaves the job alone. Restore
also refuses implicit fresh sessions on incompatible saved identities; repair
the configuration or use explicit recovery.

Unmatched jobs are reported and left running. Inspection uses effective actor
profiles and private secrets, plus original owned configurations. It does not
scan unrelated users' configurations. Permanent identity/history failures need
explicit recovery. Busy records can remain pending indefinitely; CCCC bounds
probe duration and backoff, not the duration of provider work.

This change does not replace the session-continuity policy. Its launch guard
accepts a successful continuity resume decision, including model-only drift;
model switching remains subject to that implementation's ownership checks.

## Delivery boundary and uncertain launches

Accepted handoffs are persisted before the completion queue is cleared. Work
queued before handoff remains retryable. An interrupted Claude terminal handoff
is conservatively marked `ambiguous`, even if it may have written no bytes.
Stranded claims are quarantined at shutdown/startup. Ambiguous messages remain
in the ledger/inbox and are excluded from automatic redelivery. Inspect the
provider transcript and explicitly retry if needed. These rules preserve source
messages and prevent automatic duplicates without claiming exactly-once delivery
across a PTY and a crash. Output produced during absence remains in Claude's
transcript/native viewer; it is not replayed as new CCCC output events. A resumed
observer also fences late output from the old turn until the next primary user
input, because Agent View can report idle before flushing its final transcript
records. New input restores strict transcript turn validation.

Before a detach-mode background launch, CCCC durably records launch uncertainty.
A timeout, failed scope, or missing launch identity cannot cause another automatic
background job. Confirmed exact rollback clears that fence. A reported short ID
is resolved to an exact session/workspace even on a failed scoped launch, allowing
explicit Stop/New Session to reconcile it. An unidentified launch requires
explicit operator reconciliation; CCCC cannot safely infer ownership from a
newly listed job. A proven failure to spawn restores the preceding ownership
record (or removes the new record), after checking the actor generation and
launch attempt. Timeouts and failures after spawning retain the fence. Receipt
persistence failures detach existing observers or stop an exact newly created
job; failed rollback keeps uncertainty fenced.

For an unidentified launch, first inspect the original provider configuration:

```sh
cccc actor reconcile-claude worker --group GROUP_ID
```

The command shows the saved configuration directory and any listed Claude jobs.
Listing is information for the operator; it never establishes actor ownership.
Inspect those jobs and their transcripts before explicitly acknowledging a reset:

```sh
cccc actor reconcile-claude worker --group GROUP_ID --acknowledge
cccc actor start worker --group GROUP_ID
```

The acknowledged command lists jobs again before clearing only the unidentified
launch fence. It stops no jobs, does not start the actor, and does not redeliver
messages. A subsequent Start can create another job, so reconcile any possible
previous launch manually first. If provider inspection fails, the command reports
that failure instead of showing an empty list; inspect the saved configuration
with the Claude CLI before acknowledging. Identified jobs require exact Actor
Stop, rather than this reset. Blocked lifecycle errors include the exact recovery
command.

The web actor menu exposes **Recover Claude launch** for an unidentified fence;
its dialog shows the original configuration and job list and requires an
acknowledgment. The daemon operations `actor_claude_launch_inspect` and
`actor_claude_launch_reset` require `by=user`; reset also requires acknowledgment
and the inspected actor generation, creation time, original configuration, and
launch-attempt ID. Reset holds the group dispatch lock and tries the same
per-actor `StartGuard` used by managed lifecycle operations before reading
ownership. If that guard is busy, reset returns `runtime_busy` with the recovery
command without waiting; retry after the lifecycle action finishes. If acquired,
the guard stays held through ownership read, validation, and deletion. Confirmed,
identified, or changed ownership is refused. This serializes reset with managed
launches within this daemon; it is not a filesystem lock against external
ownership-file writers.

The lock order is group dispatch lock, then a nonblocking attempt to acquire
`StartGuard`, then ownership filesystem operations. Reset performs no provider
RPC or further dispatch-lock acquisition while holding the guard. It cannot wait
on a launch that owns `StartGuard` and is waiting for the group lock.

## Host process policy

Unix detach launches use a separate process group and are excluded from CCCC's
owned-process watchdog. Linux systemd deployments first probe a disposable
`systemd-run --user --scope --quiet --collect --expand-environment=no -- /bin/true`
with a two-second deadline. Success selects the same scope transport for launch.
Literal arguments, including dollar signs in paths/settings/MCP JSON, are preserved.
Older runners that reject the expansion option fail the probe. A failed provider
launch never falls back and launches again.

When the probe fails, CCCC logs the process-group fallback. A process group does
not escape a service cgroup. Without a separate scope, unit-stop survival requires
`KillMode=process`; `KillMode=mixed` still kills remaining cgroup members. Cgroup-wide
OOM, systemd-oomd, user-manager shutdown, machine shutdown, and provider-supervisor
failure remain separate failure boundaries. Existing Claude supervisors retain
their original cgroup; this change does not relocate shared supervisors.

On macOS, operators must evaluate launchd's `AbandonProcessGroup`; CCCC changes
no launchd configuration. Windows uses a kill-on-close daemon job object; safe
selective breakaway is not implemented or validated, so detach is rejected there.

References: [systemd-run](https://github.com/systemd/systemd/blob/main/man/systemd-run.xml),
[systemd kill policy](https://github.com/systemd/systemd/blob/main/man/systemd.kill.xml),
[Apple launchd](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5),
[Windows job objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects).

## Reproducing graceful restart

On Linux with an installed, authenticated Claude CLI:

```sh
scripts/repro-claude-daemon-restart.sh /absolute/path/to/cccc detach /tmp/restart-idle idle
scripts/repro-claude-daemon-restart.sh /absolute/path/to/cccc detach /tmp/restart-busy busy
```

Use `stop` against an unmodified binary. The script uses its own disposable home,
provider configuration, socket, and workspace, copies authentication privately,
and requests tiny Haiku replies. Idle mode restarts after a reply. Busy mode
observes `tempo=active` and signals the daemon while another turn is in flight.
Both verify the same PID/job/session, actor reattachment, follow-up reply, and
retained source/delivery states. Cleanup stops only its own processes and deletes
its scratch home. `REPRO_CLAUDE_AUTH_DIR` selects the authentication source.
These checks prove idle and busy graceful survival; they do not validate real
systemd scope placement, unit-stop/OOM behavior, launchd, or Windows breakaway.
