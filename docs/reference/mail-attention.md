# Mail attention

Mail remains durable Inbox work. An attention hint advertises eligible Mail;
it never copies the message body, promotes Mail to Send or advances a cursor.
An accepted runtime handoff is evidence of submission, not evidence of a Mail
read. `mail.read` is the consumption fact.

The daemon refreshes attention on its existing 60-second unread scan. A scan
never queues input, starts an Actor or changes a Group lifecycle. The first
attention token is due five minutes after the oldest eligible source, subject
to the retained per-actor cooldown and explicit `mail_notice_after_seconds`
override. Setting that option to zero disables standalone admission while
keeping passive context offers. Each presented token advances exponential
backoff: 10, 20, 40, 80, 160 minutes, then six hours, plus stable 0–30s jitter.
Three accepted or ambiguous standalone wakeups exhaust an unresolved episode.
Partial reads, changed unread sets, new arrivals and process restarts do not
reset that episode. A fully resolved eligible set closes it; a later episode
retains the minimum five-minute actor cooldown.

Eligible sources are current-generation, concrete-recipient, unread Mail with a
valid nonfuture timestamp. Own Mail, broadcasts, replied sources and Mail with
accepted/ambiguous manual delivery do not qualify. At exactly 72 hours, Mail
expires from every automatic attention carrier while remaining in raw Inbox
and history. The journal retains an expiry watermark so a backward clock change
cannot revive expired hints. Diagnostics separate raw unread, eligible attention,
expired unread and invalid unread counts.

Bootstrap and coordination responses claim an actor-owned daemon token before
returning a hint. Ordinary Send delivery reserves its optional hint before
external input and checks eligibility again immediately before submission.
Repeated carrier IDs cannot reoffer that hint. When ordinary work and standalone
attention compete, ordinary work wins. An attention-originated turn cannot
advertise another hint, and a reminder is never itself an Inbox obligation.

The foundation adapter admits standalone attention only at an authenticated
structured-runtime `wait_next_turn`, under the Group write permit, after the
previous turn has completed and ordinary pending work has been considered.
There is no PTY output heuristic, prompt detection, steer, Control-C, auto-start
or automatic Group resume. Plain terminal Actors, including Hermes, receive
passive hints only in this patch. A native Hermes boundary adapter and observed
`mail.read` after admission are separate acceptance work; this patch does not
claim a provider consumption result.

`mail.attention` ledger facts are the authority. The atomically replaced
`state/mail-attention.json` cache is bounded by Actors and can be deleted or
rebuilt. Per-Group locking serializes token claims. A committed reservation owns
its token before input. A reservation from another daemon incarnation settles
as ambiguous, spending a standalone wakeup if applicable. Missing/corrupt cache
never resets a budget. Malformed relevant journal facts suppress optional
attention rather than guessing. Ordinary operations continue; repeated scan
diagnostics are deduplicated. Attention sources are excluded from generic
pending replay, delivery worker queues and explicit runtime turn recovery.

Migration suppresses legacy `mail_notice` jobs and seeds attention clocks and
budgets from accepted/ambiguous legacy receipts. An unresolved legacy notice
with no trustworthy receipt disables standalone attention for that episode,
while passive offers remain available. In Wick Group `g_819ab6ffb46b`, only the
identified `mail-drain` notify rule with a 300-second interval is disabled; its
exact prior definition is retained in a `legacy_rule_retired` journal fact.
Already queued events are matched by Group, notification kind and rule ID,
never by text. Other automation remains unchanged.

For rollback, use `mail_notice_after_seconds=0` to disable standalone offers
while retaining passive hints and all journal history. Do not restore the old
PTY injector: it lacks atomic admission and cannot enforce the retained budget.
