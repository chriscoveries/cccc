#!/usr/bin/env python3
"""Replay recorded notice facts through an owned foreground daemon."""
import argparse
from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import time
import traceback

parser = argparse.ArgumentParser()
parser.add_argument('--binary', required=True)
parser.add_argument('--source', required=True)
parser.add_argument('--fixture', required=True)
parser.add_argument('--output', required=True)
parser.add_argument('--candidate', action='store_true')
args = parser.parse_args()
binary = Path(args.binary).resolve()
output = Path(args.output).resolve()
output.mkdir(parents=True, exist_ok=False)
home = output / 'home'
home.mkdir()
log = (output / 'daemon.log').open('wb')
env = {**os.environ, 'CCCC_HOME': str(home), 'HOME': str(output), 'TZ': 'UTC', 'CCCC_AUTOMATION_TICK_PROBE': '1'}
daemon = subprocess.Popen([str(binary), 'daemon', 'run'], env=env, stdout=log, stderr=log)
address = home / 'daemon/ccccd.sock'
calls, checks = [], {}
exception = None

def rpc(op, **arguments):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(30)
        connection.connect(str(address))
        connection.sendall(json.dumps({'v': 1, 'op': op, 'args': arguments}).encode() + b'\n')
        result = json.loads(connection.makefile('rb').readline())
    calls.append({'at': time.time(), 'op': op, 'args': arguments, 'response': result})
    assert result.get('ok'), result
    return result['result']


def ledger(group):
    return [json.loads(line) for line in (home / 'groups' / group / 'ledger.jsonl').read_text().splitlines()]


def notices(group):
    return [event for event in ledger(group) if event['kind'] == 'system.notify' and event['data'].get('kind') == 'mail_notice']


def accepted(group, event_id):
    return any(event['kind'] == 'runtime.delivery' and event['data'].get('source_event_id') == event_id and event['data'].get('state') == 'accepted' for event in ledger(group))


try:
    deadline = time.monotonic() + 20
    while not address.exists():
        assert daemon.poll() is None, 'owned daemon exited before readiness'
        assert time.monotonic() < deadline, 'socket readiness timeout'
        time.sleep(.05)
    rpc('ping')
    group = rpc('group_create', title='Recorded notice replay', by='user')['group']['group_id']
    project = output / 'project'
    project.mkdir()
    rpc('attach', group_id=group, path=str(project), by='user')
    rpc('actor_add', group_id=group, actor_id='recipient', runtime='custom', command=['/bin/cat'], submit='none', by='user')
    rpc('actor_start', group_id=group, actor_id='recipient', by='user')
    configured = rpc('group_settings_update', group_id=group, by='user', patch={'mail_notice_after_seconds': 1, 'mail_notice_idle_after_seconds': 0, 'reply_notice_after_seconds': 0, 'mail_notice_repeat_seconds': 3600})
    rpc('group_set_state', group_id=group, state='active', by='user')
    actor = next(a for a in rpc('group_show', group_id=group)['group']['actors'] if a['id'] == 'recipient')
    history = json.loads(Path(args.fixture).read_text())
    original = next(event for event in history if event['kind'] == 'system.notify')
    original_at = datetime.fromisoformat(original['ts'].replace('Z', '+00:00'))
    anchor = datetime.now(timezone.utc) - timedelta(seconds=3570)
    ids = []
    for event in history:
        event['group_id'] = group
        recorded_at = datetime.fromisoformat(event['ts'].replace('Z', '+00:00'))
        event['ts'] = (anchor + (recorded_at - original_at)).isoformat()
        if event['kind'] == 'system.notify':
            event['data']['context']['actor_created_at'] = actor['created_at']
        if event['kind'] == 'chat.message':
            ids.append(event['id'])
        rpc('event_append', event=event)
    checks['captured_notice_accepted_and_mail_unread'] = accepted(group, original['id']) and len(rpc('inbox_peek', group_id=group, actor_id='recipient', by='recipient')['messages']) == 2
    time.sleep(3)
    checks['no_early_repeat'] = len(notices(group)) == 1
    # The unchanged production scheduler runs the unread pass every 60 seconds.
    deadline = time.monotonic() + 90
    while len(notices(group)) == 1 and time.monotonic() < deadline:
        assert daemon.poll() is None, 'daemon exited during unread pass'
        time.sleep(.25)
    all_notices = notices(group)
    checks['periodic_repeat_with_pending_later_mail'] = len(all_notices) == 2 and all_notices[-1]['data']['context']['source_event_ids'] == ids
    if checks['periodic_repeat_with_pending_later_mail']:
        repeated_id = all_notices[-1]['id']
        deadline = time.monotonic() + 15
        while not accepted(group, repeated_id) and time.monotonic() < deadline:
            time.sleep(.1)
        checks['repeat_accepted_through_real_pty'] = accepted(group, repeated_id)
        checks['repeat_keeps_mail_unread'] = len(rpc('inbox_peek', group_id=group, actor_id='recipient', by='recipient')['messages']) == 2
        consumed = rpc('inbox_read', group_id=group, actor_id='recipient', by='recipient')
        checks['read_consumed_exactly_pending_mail'] = [event['id'] for event in consumed['messages']] == ids
        rpc('group_settings_update', group_id=group, by='user', patch={'mail_notice_repeat_seconds': 1})
        time.sleep(65)
        checks['no_repeat_on_next_unread_pass_after_read'] = len(notices(group)) == 2
    else:
        checks['repeat_accepted_through_real_pty'] = False
    checks['real_periodic_unread_path_exercised'] = 'CCCC_UNREAD_TICK' in (output / 'daemon.log').read_text()
except Exception:
    exception = traceback.format_exc()
finally:
    clean_shutdown = False
    try:
        assert daemon.poll() is None and address.exists(), 'daemon exited before requested shutdown'
        rpc('shutdown')
        daemon.wait(timeout=20)
        clean_shutdown = daemon.returncode == 0
    except Exception:
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.wait(timeout=10)
    log.close()
    report = {'source': args.source, 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'fixture_sha256': hashlib.sha256(Path(args.fixture).read_bytes()).hexdigest(), 'probe_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'checks': checks, 'exception': exception, 'calls': calls, 'daemon_stopped': daemon.poll() is not None, 'clean_shutdown': clean_shutdown}
    semantic_failures = {key for key, passed in checks.items() if not passed}
    verdict = bool(checks) and not exception and all(checks.values()) and report['daemon_stopped'] and report['clean_shutdown']
    if args.candidate:
        matched = verdict
    else:
        matched = not exception and report['daemon_stopped'] and report['clean_shutdown'] and semantic_failures == {'periodic_repeat_with_pending_later_mail', 'repeat_accepted_through_real_pty'} and checks.get('real_periodic_unread_path_exercised') is True
    report.update(verdict='PASS' if verdict else 'FAIL', expected_result_matched=matched)
    (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({key: report[key] for key in ('source', 'checks', 'verdict', 'expected_result_matched', 'exception')}), flush=True)
raise SystemExit(0 if matched else 1)
