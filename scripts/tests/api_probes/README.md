# Recorded daemon API probes

These probes launch a private foreground daemon, call its Unix socket, retain raw
requests/responses, and stop the owned process. The workflow builds four pinned
sources: the released baseline, upstream main, and the two feature candidates.
A baseline run must fail named feature assertions after successful setup; a
candidate must pass every assertion. Setup errors are never negative evidence.

`tasks.json` projects three recorded task files from 2026-10-22. It retains IDs,
status, priority, waiting state and timestamps, and omits private titles, notes
and assignees. Revision fields are absent from the captured input.

`events.json` projects 102 consecutive recorded ledger events from 2026-10-22.
It retains event IDs, timestamps, kinds and ordering. Group and actor identifiers
use consistent aliases; message content and unrelated payload fields are omitted.
The recipient and time-bound expectations are derived from these recorded rows.

Operations performed during a probe are explicit test mutations of these inputs;
they are not represented as captured historical events. Binary, fixture, source
and probe hashes are retained with each result.
