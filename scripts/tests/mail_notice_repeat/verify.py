#!/usr/bin/env python3
"""Reject compile/setup failures and unexpected negative-reference failures."""
import argparse
from pathlib import Path
import re

parser = argparse.ArgumentParser()
parser.add_argument('--logs', type=Path, required=True)
parser.add_argument('--candidate', action='store_true')
args = parser.parse_args()
expected = {
    'core': {
        'automation::mail_notice_repeat_tests::mail_notice_repeat_carries_both_captured_pending_mail',
        'automation::mail_notice_repeat_tests::mail_notice_repeat_warm_projection_does_not_replay_history',
    },
    'daemon': {
        'ops::automation_runtime::mail_notice_repeat_tests::mail_notice_repeat_daemon_accepts_repeat_with_later_mail',
        'ops::automation_runtime::mail_notice_repeat_tests::mail_notice_repeat_daemon_stops_after_read',
        'ops::automation_runtime::mail_notice_repeat_tests::mail_notice_repeat_daemon_stops_after_reply',
        'ops::automation_runtime::mail_notice_repeat_tests::mail_notice_repeat_daemon_stops_after_mail_delivery',
        'ops::automation_runtime::mail_notice_repeat_tests::mail_notice_repeat_daemon_stops_after_actor_generation_change',
    },
}
for target, count in [('core', 4), ('daemon', 6)]:
    log = (args.logs / f'{target}.log').read_text()
    code = int((args.logs / f'{target}.exit').read_text())
    summary = re.findall(r'test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;', log)
    assert len(summary) == 1, (target, 'missing/ambiguous completed test result')
    passed, failed, ignored = map(int, summary[0])
    assert passed + failed == count and ignored == 0, (target, summary)
    failures = set(re.findall(r'^test (\S+) \.\.\. FAILED$', log, re.M))
    if args.candidate:
        assert code == 0 and passed == count and failed == 0 and not failures, (target, code, failures)
    else:
        assert code == 101 and failures == expected[target] and failed == len(failures), (target, code, failures)
    print(target, 'candidate PASS' if args.candidate else 'exact new-test RED confirmed', passed, failed)
