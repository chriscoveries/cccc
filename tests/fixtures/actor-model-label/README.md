# Captured Actor declaration fixture

Captured at `2026-10-24T21:19:39.456430+00:00` from persisted CCCC Actor state. Source snapshot SHA-256:
`be979d347022a99be900fa35b196c4bb1baffc851d3a75677be42dddfc968e86`.

`actor.json` projects one existing Actor's `runtime`, `command`, `submit`,
`enabled`, `runner` and `runtime_state_source` without changing their values.
The Actor ID is replaced with the consistent public alias `worker`; titles,
workspace paths, environment, profile details and unrelated fields are omitted.
The source Actor was disabled, as retained in this fixture. The private source
snapshot and field mapping are retained outside the repository.

The captured Actor had no `model_label`. `labels.json` contains the public model
tokens from this Actor's command and a second Actor's command in the same
capture. Tests declare those values as explicit operator actions. These are
controlled declarations, not observed serving models. Validation boundary cases,
updates, omission and clearing are explicit test operations on the recorded
base. The fixture does not require launching either provider.
