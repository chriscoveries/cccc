#!/usr/bin/env python3
"""Require a complete real-daemon result, including meaningful old-source failures."""
import argparse
import hashlib
import json
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('--report', required=True)
parser.add_argument('--source', required=True)
parser.add_argument('--candidate', action='store_true')
args = parser.parse_args()
path = Path(args.report)
report = json.loads(path.read_text())
assert report['source'] == args.source
assert report['exception'] is None, report['exception']
assert report['daemon_stopped'] is True
assert len(report['calls']) >= 10
checks = report['checks']
assert len(checks) == 8, checks
assert all(isinstance(value, bool) for value in checks.values())
failed = sorted(name for name, passed in checks.items() if not passed)
if args.candidate:
    assert not failed, failed
else:
    # A missing report, broken transport, or failed setup is not a negative proof.
    assert failed
    if report['feature'] == 'task-revision':
        assert checks['whole_context_guard_remains_independent']
        assert checks['stale_guard_writes_nothing']
        assert checks['late_conflict_discards_batch']
        assert 'legacy_revision_zero' in failed
        assert 'stale_revision_conflict_names_task' in failed
    else:
        assert 'recipient_and_interval' in failed
        assert 'absent_recipient_empty' in failed
print(json.dumps({'result': 'PASS' if args.candidate else 'EXPECTED_RED', 'source': args.source, 'feature': report['feature'], 'failed_checks': failed, 'report_sha256': hashlib.sha256(path.read_bytes()).hexdigest()}))
