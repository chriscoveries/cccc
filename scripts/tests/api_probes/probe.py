#!/usr/bin/env python3
"""API probes against an owned disposable daemon using recorded fixture projections."""
import argparse
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
parser.add_argument('--sha256', required=True)
parser.add_argument('--feature', choices=['task-revision', 'ledger-search-filters'], required=True)
parser.add_argument('--fixture', required=True)
parser.add_argument('--output', required=True)
args = parser.parse_args()
binary = Path(args.binary).resolve()
assert hashlib.sha256(binary.read_bytes()).hexdigest() == args.sha256
output = Path(args.output).resolve()
output.mkdir(parents=True, exist_ok=False)
home = output / 'home'
home.mkdir()
env = {**os.environ, 'CCCC_HOME': str(home), 'HOME': str(output), 'TZ': 'UTC'}
log = (output / 'daemon.log').open('wb')
daemon = subprocess.Popen([str(binary), 'daemon', 'run'], env=env, stdout=log, stderr=log)
socket_path = home / 'daemon/ccccd.sock'
calls = []
checks = {}
exception = None


def rpc(op, **arguments):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(30)
        sock.connect(str(socket_path))
        sock.sendall(json.dumps({'v': 1, 'op': op, 'args': arguments}).encode() + b'\n')
        response = json.loads(sock.makefile('rb').readline())
    calls.append({'op': op, 'args': arguments, 'response': response})
    return response


def ok(op, **arguments):
    response = rpc(op, **arguments)
    assert response.get('ok'), response
    return response['result']


try:
    for _ in range(200):
        if socket_path.exists():
            break
        if daemon.poll() is not None:
            raise RuntimeError('owned daemon exited before socket readiness')
        time.sleep(0.05)
    ok('ping')
    group = ok('group_create', title='API probe')['group']['group_id']
    if args.feature == 'task-revision':
        directory = home / 'groups' / group / 'context/tasks'
        directory.mkdir(parents=True, exist_ok=True)
        for task in json.loads(Path(args.fixture).read_text()):
            (directory / (task['id'] + '.yaml')).write_text(json.dumps(task))
        read = ok('task_list', group_id=group, task_id='T792')
        checks['legacy_revision_zero'] = read['task'].get('rev') == 0
        version = ok('context_get', group_id=group)['version']
        ok('context_sync', group_id=group, by='user', ops=[{'op': 'agent_state.update', 'actor_id': 'peer', 'focus': 'unrelated context'}])
        patch = {'op': 'task.update', 'task_id': 'T792', 'waiting_on': 'none', 'if_task_rev': 0}
        whole = rpc('context_sync', group_id=group, by='user', if_version=version, ops=[patch])
        checks['whole_context_guard_remains_independent'] = whole.get('error', {}).get('code') == 'version_conflict'
        accepted = rpc('context_sync', group_id=group, by='user', ops=[patch])
        checks['unrelated_write_keeps_task_guard_valid'] = bool(accepted.get('ok'))
        path = directory / 'T792.yaml'
        before = path.read_bytes()
        stale = rpc('context_sync', group_id=group, by='user', ops=[patch])
        checks['stale_revision_conflict_names_task'] = stale.get('error', {}).get('code') == 'task_conflict' and stale['error'].get('details', {}).get('task_id') == 'T792'
        checks['stale_guard_writes_nothing'] = path.read_bytes() == before
        prior = {p.name: p.read_bytes() for p in directory.glob('*.yaml')}
        atomic = rpc('context_sync', group_id=group, by='user', ops=[{'op': 'task.update', 'task_id': 'T898', 'waiting_on': 'user', 'if_task_rev': 0}, patch])
        checks['late_conflict_discards_batch'] = not atomic.get('ok') and {p.name: p.read_bytes() for p in directory.glob('*.yaml')} == prior
        created = rpc('context_sync', group_id=group, by='user', ops=[{'op': 'task.create', 'title': 'revision probe'}])
        rows = ok('task_list', group_id=group)['tasks']
        checks['new_task_revision_one'] = created.get('ok') and next(t for t in rows if t.get('title') == 'revision probe').get('rev') == 1
        invalid = rpc('context_sync', group_id=group, by='user', ops=[{'op': 'task.update', 'task_id': 'T792', 'if_task_rev': '0'}])
        checks['invalid_guard_rejected'] = invalid.get('error', {}).get('code') == 'invalid_args'
    else:
        events = json.loads(Path(args.fixture).read_text())
        for event in events:
            event['group_id'] = group
            ok('event_append', event=event)
        selected = [e for e in events if 'actor-03' in e['data'].get('to', [])]
        since, until = selected[1]['ts'], selected[-2]['ts']
        expected = [e['id'] for e in selected if since <= e['ts'] <= until]
        query = {'group_id': group, 'kind': 'chat', 'to': 'actor-03', 'since_ts': since, 'until_ts': until, 'limit': 200}
        result = ok('ledger_search', **query)
        checks['recipient_and_interval'] = [e['id'] for e in result['events']] == expected
        result = ok('ledger_search', **{**query, 'limit': 1})
        checks['filter_before_latest_limit'] = [e['id'] for e in result['events']] == expected[-1:] and result['has_more'] is True
        result = ok('ledger_search', **{**query, 'after': events[0]['id'], 'limit': 1})
        checks['forward_pagination'] = [e['id'] for e in result['events']] == expected[:1]
        result = ok('ledger_search', group_id=group, to='unknown-recipient')
        checks['absent_recipient_empty'] = result['events'] == []
        for field, value in [('since_ts', 'yesterday'), ('until_ts', 1), ('to', 17)]:
            result = rpc('ledger_search', group_id=group, **{field: value})
            checks['invalid_' + field] = result.get('error', {}).get('code') == 'invalid_args'
        result = rpc('ledger_search', group_id=group, since_ts=until, until_ts=since)
        checks['reversed_interval_rejected'] = result.get('error', {}).get('code') == 'invalid_args'
except Exception:
    exception = traceback.format_exc()
finally:
    daemon.terminate()
    try:
        daemon.wait(timeout=20)
    except subprocess.TimeoutExpired:
        daemon.kill()
        daemon.wait(timeout=10)
    log.close()
    report = {'binary': str(binary), 'sha256': args.sha256, 'source': args.source, 'fixture_sha256': hashlib.sha256(Path(args.fixture).read_bytes()).hexdigest(), 'probe_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'feature': args.feature, 'exception': exception, 'checks': checks, 'calls': calls, 'daemon_exit': daemon.returncode, 'daemon_stopped': daemon.poll() is not None}
    (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({'feature': args.feature, 'checks': checks, 'report': str(output / 'report.json')}), flush=True)
raise SystemExit(0 if not exception and checks and all(checks.values()) else 1)
