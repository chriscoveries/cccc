# Opt-in task dispatch

The Automation panel can add a `dispatch_ready_task` action to an existing
interval, cron, or one-time rule. New dispatch rules start disabled. Enable
the rule explicitly to assign work without a coordinator Actor.

An Actor is eligible only while its current managed session reports idle, or
an enabled native turn probe reports idle for the same live PTY generation.
Output silence and an unknown PTY working state are never evidence of idle.
Disabled, stopped, busy, and approval-waiting Actors are excluded.

Each pass considers open or planned, unassigned tasks in creation order, with
task id breaking ties. A task must have no unmet blockers or external wait,
and its recorded cost must remain below its optional target budget. Reaching
a target prevents a new assignment; it never stops a running turn.

Actors are ordered by per-million-token price, then descending strength, then
Actor id. Unknown prices sort last; unknown strength sorts after known
strength at the same price. An unknown or subscription-only price cannot
satisfy `max_usd_per_mtok`. A monthly subscription is not converted into a
token price. Model metadata comes from the existing model catalog.

Each Actor receives at most one new task per pass and cannot receive another
while it holds an open, planned, or active task. The optional action field
`dispatch_cooldown_ms` defaults to 60000 and applies across dispatch rules.
The assignment retains the task's other fields, including cost and provenance.
It records `task.assign` with the rule id and Actor generation, together with
the notification that pushes the task to that Actor. That notification carries
exactly one structured `task_ref` naming the assigned card. Delivery-bound cost
attribution uses that ref; the runtime's turn token keeps its existing meaning.
A delivery with zero or multiple task refs is unattributed.

Task changes, cooldown state, and the notification ledger batch share a durable
undo journal. The context lock serializes selection and assignment; a single
ledger writer lock covers the batch. Ordinary `ledger::append` is never called
while holding the context lock. A complete ledger suffix commits the batch;
partial writes restore every before-image. Recovery runs before daemon runtime
restoration or listeners, and before context reads or writes. Committed notices
use the existing pending-delivery and delivery-claim machinery, including its
existing treatment of ambiguous delivery after interruption.

The separate **Notify idle Actors about held tasks** checkbox defaults off.
It emits one passive notice for each changed set of held open tasks, without
assignment changes or an escalation ladder. Unread Mail continues to use the
existing `mail_notice_idle_after_seconds` behavior.

The dispatch tests replay the existing redacted task capture. Setup changes
assignment, wait, blocker, and budget fields through task operations rather
than constructing a new workload. Selection tests use captured Actor identities
and revision tokens; these tokens are test inputs to the pure task planner,
not evidence of a live runtime. Runtime admission uses actual managed-session
or PTY generation identity. Transaction controls interrupt partial and complete
ledger appends to verify rollback and recovery separately.
