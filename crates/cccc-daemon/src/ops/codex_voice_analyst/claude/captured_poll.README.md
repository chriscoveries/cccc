The state and four transcript records are projections of a real native session
retained in the incident evidence (state timestamp 2026-10-13). The state retains
its actual CLI version, timestamp and
transcript filename. Its private directory prefix is replaced consistently.
Transcript projections retain only actual record types, session IDs and ancestry
IDs; prompts, assistant bodies, commands and environment values are omitted.
The private provenance packet retains the source hashes and exact field mapping.

Tests copy these records into a disposable projects directory. Equal-length path
mutation, preserved mtime, torn state, transcript replacement and symlink changes
are explicit adversarial filesystem controls, not claimed captured incidents.
This fixture proves polling/decode behavior, not recovery authority or live-holder
absence. Existing native session/resume/relocation behavior is preserved.
