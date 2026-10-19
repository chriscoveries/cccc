# Real context capture — cwflab (g_1a9e39785e1a)

`real-context-g1a9e39785e1a.zip` is a verbatim capture of the cwflab group's
context document: 775 real task cards and 43 real agent states, taken from the
live daemon on firestar on 2026-10-19 while the group was mid-flight.

The unbounded `context_get` projection of this document is ~4.3 MB. That is the
payload behind the incident in owner spec `dd17cbba`: a DSH actor called
`cccc_context_get`, the answer was ~4 MB, the call failed, and CCCC retried
until the session corrupted.

It is committed because the regression it guards is a size regression: a
hand-built fixture with a handful of cards never reproduces it, and the paging
order assertions only mean something against the real board order.

## Regenerating

Capture the raw answer from a live daemon (read-only) and fold it into the
document shape the store loads:

```sh
curl -s "http://127.0.0.1:8848/api/v1/groups/g_1a9e39785e1a/context?detail=full" \
  -o /tmp/ctx_full.json

python3 - <<'PY'
import json, zipfile
d = json.load(open('/tmp/ctx_full.json'))['result']
doc = {
    'v': 1, 'revision': 0,
    'tasks_revision': int(d['tasks_version'].split(':')[1]),
    'updated_at': '',
    'coordination': {k: d['coordination'][k]
                     for k in ('brief', 'recent_decisions', 'recent_handoffs')},
    'tasks': d['coordination']['tasks'],
    'agent_states': {s['id']: {k: v for k, v in s.items() if k != 'id'}
                     for s in d['agent_states']},
    'meta': d['meta'],
}
raw = json.dumps(doc, separators=(',', ':'), sort_keys=True).encode()
with zipfile.ZipFile('real-context-g1a9e39785e1a.zip', 'w',
                     zipfile.ZIP_DEFLATED, compresslevel=9) as z:
    z.writestr('real-context-g1a9e39785e1a.json', raw)
PY
```

The tasks, agent states and brief in the archive are the captured values; only
the revision metadata is normalised so the fixture is stable.
