# Captured turns: two cards, two deliveries, one managed Actor

`captured_turns.json` is a redacted capture of two consecutive assigned turns of
one managed Pi Actor, taken 2026-10-23 between 04:24:55Z and 04:25:30Z. Each
delivery carried exactly one task ref, and each turn reported its own usage.

What is kept: the two board task ids and titles as the board had them, the ref
object each delivery carried (`kind`, `task_id`, `title`), the Actor runtime's
own task and turn token for each turn, the provider's token totals for the turn
(source, model, input, output, cache-read, cache-write, sample count), and the
per-million-token price the operator's catalog carried for that model at the
time.

What is not kept: the host, its paths, the Actor's name, the provider home, the
prompt text, and anything the turn produced. Nothing identifies the machine the
capture was taken on.

The point of the capture is a defect it showed: the usage record's `task_id` was
the runtime's own turn token (`pi-…`), not a board task, so the money a turn cost
was never charged to the card the delivery named. The tests read these rows, one
per delivery, and assert the money lands on that card.
