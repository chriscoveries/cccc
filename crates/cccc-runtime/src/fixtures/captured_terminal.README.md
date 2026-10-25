Real terminal startup bytes recorded on 2026-10-20, from a native TUI process.
The recording tool's command header, visible working directory and provider
label are redacted. All other bytes, including ANSI queries, bracketed-paste
controls, carriage returns and Unicode, are retained. The private capture packet
holds original bytes, digest and the exact redaction map.

Tests replay this one stream at different read boundaries and capacities; they
never generate terminal traffic or treat output as authoritative turn state.
