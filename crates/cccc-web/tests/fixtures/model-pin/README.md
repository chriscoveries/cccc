# Minimal picker DOM captures

Two real, read-only selected-state captures from 12 November 2026. The original
capture opened each existing picker, read accessible attributes and closed it;
no model was selected and no prompt was submitted. The projection retains only
the model radios, slider bounds/value, status text and picker button semantics.
Generated ids, CSS classes, conversation prefixes and all account/environment
identifiers are removed. No prompts, credentials or conversation text remain.

These captures prove recognition of the already-selected presets, not safe
cross-family transitions. In particular, the unchecked family exposes no
remembered effort. Automated selection is deliberately absent. The contradictory
`medium` data attribute on the selected Pro capture is retained so tests cannot
mistake it for the model's actual accessible selection.

Tests render this captured structure with a minimal composer/form harness. The
composer's opaque control text comes from a captured status label; it is not a
claimed message/turn capture. Busy, denied, disabled, missing and drift states
are explicitly labelled fault mutations. No test enters any model on an account.
