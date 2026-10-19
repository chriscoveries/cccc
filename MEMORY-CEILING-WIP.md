# Native ACP memory ceiling — T620 correction

The earlier handover in this file is superseded by the implementation and regressions in this branch. Validation remains pending; no runtime PASS is claimed here.

`Actor.memory_limit_mb` defaults to zero. A configured ceiling currently enforces only directly owned, single-child native ACP sessions: Copilot, Devin, Cursor, Kimi, Auggie, Kiro, Hermes and Droid in ACP mode. PTY/viewer and multi-root runtimes are skipped. A configured limit is not a promise of enforcement for an unsupported topology.

The first idle/RSS observations are filters. Final admission reloads the current Profile/private environment, verifies a materialized receipt for the live conversation, and compares a new owned-tree RSS sample with the current nonzero limit. Controlled delivery uses the same nonblocking start guard as maintenance. The serialized ACP command loop refuses maintenance while work, RPCs, approvals or native input remain pending, and closes further admission before acknowledging an idle retirement.

A durable exact-resume sidecar is written before stopping. Missing or changed receipts cannot fall back to session/new, including ordinary automatic recovery after a failed maintenance attempt or daemon restart. Only verified matching resume and attachment, or an explicit New Session, clears the requirement. The existing owned ChildOwner/OwnedProcessTree controls shutdown; numeric RSS sampling PIDs are never used as independent process authority.

Cooldown reservations use atomic durable writes and survive failure/restart. Corrupt cooldown history suppresses enforcement. One outcome event per attempted restart remains in the ledger; the available concrete foreman is targeted through normal durable non-chat delivery. No available foreman means ledger-only evidence, never broadcast.

Regressions cover authoritative idle-close, busy/pending refusal, owned root versus viewer projection, stale generations, Profile drift, fresh limits, receipt removal/replacement races, ordinary recovery gating, matching resume, durable requirements, cooldown corruption and concrete notice routing. They must execute through the required fork CI before semantic PASS. Staging proof, fresh RSS comparison and production acceptance remain separate gates.

No firestar or MACDAD cap, install or restart is authorized by this branch.
