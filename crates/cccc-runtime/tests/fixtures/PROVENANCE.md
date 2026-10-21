# hermes-pty-startup-claw-stage.pty

A real PTY byte stream, recorded from the claw stage on 2026-10-20, used by the
activity-tracker tests. Nothing in it is written by hand: it is what the kernel
handed back from a real terminal.

- source host: claw (`/srv/cccc-stage` is that host's cccc stage)
- recorded as: `ccstage`, `cd /tmp && timeout 12 script -q -c "…" /tmp/hcap2.pty`
- command inside the PTY: `/srv/cccc-stage/bin/hermes --cli --yolo --provider cpa -m step-5-preview-free`
  (the same command the stage's Hermes actors launch, per `cccc actor list --group g_a9002090f4ab`)
- recorded at: 2026-10-20 18:59:20+02:00 (the `Script started` header line `script` writes is part of the capture and is kept byte-exact)
- bytes: 7340
- sha256: `ab13b074f4ea586514d467579b6bea57bfa3880fe21a8c9a92c84caece08bd99`

Why this stream: it is a real TUI's startup traffic — cursor/OSC queries
(`ESC]11;?`, `ESC[c`), a bracketed-paste enable, colour sequences, and prose
lines. That mix is what the byte floor in `pty_liveness` exists for: a live lane
emits kilobytes when it is working, and only stray control sequences when it is
repainting an idle screen.

The test that replays it (`output_reader.rs`, `recorded_hermes_startup_stream_is_observed_as_output`)
asserts the tracker observed the whole capture and stamped `last_output`, and
that a snapshot taken from that replay reads as working. Timing is not recorded
in a byte stream, so the idle branch is exercised by the pure `pty_liveness`
function over explicit ages — a control, labelled as such, not a second fixture.
