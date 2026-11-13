# Native completion replay

Captured from one owned Devin TUI session on 2026-11-13. No other workspace
conversation was read. The private originals remain with the operator.

`completed.json` is a consumer projection of 29 native message nodes, the
actual pre-input completion step48 and post-input completion step53, the
session creation value and the ATIF-v1.7/session header. Active tip ancestry
joins the later completion to its nearest new user and through the earlier
tip. Both completion contents were checked against their transcript records
before redaction. Native step IDs, parents, roles, times, token counts and
tool-call presence/count are retained; a session's creation time is not used
as a process-start witness.

Every session/message identity is replaced. All prompt, completion, system
and tool text is replaced by consistent placeholders. Tool names, arguments,
reasoning, model metadata, environment, paths and inventories are omitted.
Placeholders preserve equality relationships; none is captured user text.
The test supplies its own temporary working directory and projects only
the consumed database columns and captured unique session/node index.

Private input checksums:

| Record | SHA256 |
|---|---|
| Before chain and exact earlier completion | a3e55356aa8e31fb4172e37168db00a5b7bc0f222062fa7de0f4660bdc40df33 |
| Pinned later chain | 17e712abfba7bf0417fe3d016abd441515c59107f820f0ae372a1ff8e583b721 |
| Native transcript header | 68f032ae1020f10dabce7916ada2ece84b58d355b6d2d64fe134e7976ca35b20 |
| Retained later transcript steps | 184c660ae9241e72e2c531c766beb9d539864d131fbfa3162c9d31333b0ef72f |

The tests explicitly mutate these records for duplicate completion,
payload/epoch mismatch, a user without completion, missing ancestor,
partial/oversized/foreign-schema transcript and input failure. These are
controlled faults, not claimed live incidents. Replay does not establish a
live notification, process-binding or service-installation proof; those
require an attended run of the exact built candidate.

Admission controls use redacted captured identities as opaque control strings
and explicitly expand the captured payload for the capacity bound. They do
not present native message IDs as CCCC transport source IDs or claim a live
4096-delivery trace.
The source-tuple suffix used by the new delivery path is a protocol-control
projection; the retained native capture predates that suffix. Its full live
submission and completion join still requires the exact candidate proof.
