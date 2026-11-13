# Mail notice with a later unread source

The adjacent JSON is a minimized real ledger capture from 2026-11-13:
one Mail, its notice and claimed/accepted handoff, then a later Mail while
the earlier notice's source remained unread. The later Mail did not receive
a notice after its configured delay.

Actor, sender, group and event identifiers are consistent aliases. Message
bodies and fields unused by notice projection are removed. Event timestamps,
ordering, recipient relationships, Actor creation time, transport and handoff
states are preserved. No read or successful agent response is invented.

The replay translates the captured timeline to a current anchor while retaining
relative times. Derived crash/ambiguity controls in the test module are labelled
as mutations; they are not represented as captured provider outcomes.
