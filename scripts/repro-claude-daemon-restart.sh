#!/bin/bash
# Usage: repro-claude-daemon-restart.sh /absolute/path/to/cccc stop|detach evidence-directory [idle|busy]
set -euo pipefail
exec python3 - "$@" <<'PY'
import hashlib, json, os, pathlib, shutil, signal, socket, subprocess, sys, tempfile, time

binary = pathlib.Path(sys.argv[1]).resolve()
expected = sys.argv[2]
assert expected in ('stop', 'detach')
evidence = pathlib.Path(sys.argv[3]).resolve()
evidence.mkdir(parents=True, exist_ok=True)
variant = sys.argv[4] if len(sys.argv) > 4 else 'idle'
assert variant in ('idle', 'busy')
root = pathlib.Path(tempfile.mkdtemp(prefix='cccc-repro-massdeath-'))
home, config, workspace = (root / name for name in ('cccc', 'claude', 'workspace'))
for path in (home, config, workspace): path.mkdir(mode=0o700)
env = {k: v for k, v in os.environ.items() if not k.startswith(('CCCC_', 'CLAUDE'))}
env.update(CCCC_HOME=str(home), CLAUDE_CONFIG_DIR=str(config), CCCC_LAUNCHER_PATH=str(binary))
source = pathlib.Path(os.environ.get('REPRO_CLAUDE_AUTH_DIR', str(pathlib.Path.home()/'.claude')))
for name in ('.credentials.json',):
    if (source/name).exists(): shutil.copy2(source/name, config/name)
# Private copies only. No writes to the user's provider configuration.
original_settings = json.loads((source/'settings.json').read_text()) if (source/'settings.json').exists() else {}
settings = {'skipDangerousModePermissionPrompt': True, 'env': {k:v for k,v in original_settings.get('env', {}).items() if k.startswith('ANTHROPIC_') or k in ('HTTP_PROXY','HTTPS_PROXY','ALL_PROXY','NO_PROXY')}}
(config/'settings.json').write_text(json.dumps(settings))
provider_config = {'hasCompletedOnboarding': True, 'projects': {str(workspace): {'hasTrustDialogAccepted': True}}}
for source_config in (source/'.claude.json', pathlib.Path.home()/'.claude.json'):
    if source_config.exists():
        original = json.loads(source_config.read_text())
        for key in ('oauthAccount', 'hasCompletedOnboarding', 'lastOnboardingVersion'):
            if key in original: provider_config[key] = original[key]
(config/'.claude.json').write_text(json.dumps(provider_config))
daemon = None
job = None
control_dir = pathlib.Path('/tmp')/f'cc-daemon-{os.getuid()}'/hashlib.sha256(str(config).encode()).hexdigest()[:8]

def wait(check, what, seconds=90):
    deadline = time.monotonic()+seconds
    while time.monotonic() < deadline:
        result = check()
        if result: return result
        time.sleep(.1)
    raise AssertionError('timed out: '+what)

def ipc(op, **args):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(40)
        connection.connect(str(home/'daemon/ccccd.sock'))
        connection.sendall((json.dumps({'v':1, 'op':op, 'args':args})+'\n').encode())
        reply = json.loads(connection.makefile('rb').readline())
    assert reply['ok'], reply.get('error')
    return reply['result']

def launch(number):
    log = open(evidence/f'daemon-{number}.log', 'w')
    process = subprocess.Popen([str(binary), 'daemon', 'run'], env=env, cwd=workspace, stdout=log, stderr=log, start_new_session=True)
    log.close()
    wait(lambda: (home/'daemon/ccccd.sock').exists() and process.poll() is None, 'daemon socket', 20)
    return process

def live(pid):
    try:
        return pathlib.Path(f'/proc/{pid}/stat').read_text().split(') ')[1][0] != 'Z'
    except FileNotFoundError: return False

def state():
    files = list(config.glob('jobs/*/state.json'))
    return json.loads(files[0].read_text()) if files else None

def reply_seen(token):
    for path in config.glob('projects/**/*.jsonl'):
        for line in path.read_text().splitlines():
            try: record = json.loads(line)
            except json.JSONDecodeError: continue
            if record.get('type') == 'assistant' and token in json.dumps(record.get('message',{})): return True
    return False

def snapshot(phase):
    result = {"phase":phase, "job_state":state()}
    if "group" in globals():
        try: result["actors"] = ipc("actor_list", group_id=group)
        except Exception as error: result["actor_error"] = str(error)
        ledger = home/"groups"/group/"ledger.jsonl"
        if ledger.exists():
            result["events"] = [json.loads(line) for line in ledger.read_text().splitlines()]
        headless = home/"groups"/group/"state/headless/events.jsonl"
        if headless.exists():
            result["headless"] = [json.loads(line) for line in headless.read_text().splitlines()]
    result["transcript"] = []
    for path in config.glob("projects/**/*.jsonl"):
        for line in path.read_text().splitlines():
            try: record = json.loads(line)
            except json.JSONDecodeError: continue
            if record.get("type") in ("user", "assistant", "system"):
                result["transcript"].append(record)
    (evidence/(phase+".json")).write_text(json.dumps(result, indent=2)+"\n")

def delivery(token):
    records = [json.loads(line) for line in (home/'groups'/group/'ledger.jsonl').read_text().splitlines()]
    sources = [e for e in records if e['kind'] == 'chat.message' and e.get('by') == 'user' and token in e.get('data',{}).get('text','')]
    assert len(sources) == 1, sources
    event_id = sources[0]['id']
    states = [e['data']['state'] for e in records if e['kind'] == 'runtime.delivery' and e['data'].get('source_event_id') == event_id]
    return {'event_id':event_id, 'states':states, 'source_retained':True}

try:
    daemon = launch(1)
    (home/'settings.yaml').write_text('runtime:\n  claude_daemon_exit: detach\n')
    group = ipc('attach', path=str(workspace))['group_id']
    ipc('actor_add', group_id=group, actor_id='worker', runtime='claude', command=['claude', '--model', 'haiku'], env={'CLAUDE_CONFIG_DIR':str(config)})
    ipc('group_start', group_id=group, by='user')
    ipc('send', group_id=group, by='user', to=['worker'], text='Reply with exactly FIRST_OK. Do not use tools.', message_mode='send')
    wait(lambda: reply_seen('FIRST_OK'), 'first tiny reply')
    before = wait(lambda: (s if (s := state())['tempo'] == 'idle' else None), 'first turn idle')
    job = before['daemonShort']
    # Linux exposes the actual worker PID in the CLI's durable job metadata.
    candidates = []
    for path in (config/'jobs'/job).glob('*.json'):
        data = json.loads(path.read_text())
        for key in ('pid', 'workerPid', 'processId'):
            if isinstance(data.get(key), int) and live(data[key]): candidates.append(data[key])
    if not candidates:
        for proc in pathlib.Path('/proc').iterdir():
            if not proc.name.isdigit(): continue
            try:
                environ = (proc/'environ').read_bytes().split(b'\0')
                cmd = (proc/'cmdline').read_bytes()
                if f'CLAUDE_JOB_DIR={config}/jobs/{job}'.encode() in environ and b'claude' in cmd: candidates.append(int(proc.name))
            except (OSError, PermissionError): pass
    assert candidates, 'could not identify exact provider job PID'
    pid = candidates[0]
    session = before['sessionId']
    receipt = home/'groups'/group/'state/runtime_sessions/worker.json'
    assert json.loads(receipt.read_text())['provider_session_id'] == session
    if variant == 'busy':
        wait(lambda: state()['tempo'] == 'idle', 'idle before busy probe')
        ipc('send', group_id=group, by='user', to=['worker'], text='Think briefly, then reply with exactly BUSY_OK. Do not use tools.', message_mode='send')
        before = wait(lambda: (s if (s := state())['tempo'] == 'active' else None), 'turn in flight')
        assert before['sessionId'] == session and before['daemonShort'] == job
    print(json.dumps({'phase':'before', 'variant':variant, 'daemon_pid':daemon.pid, 'job':job, 'pid':pid, 'session_id':session, 'tempo_at_signal':before['tempo']}), flush=True)
    daemon.send_signal(signal.SIGTERM)
    daemon.wait(timeout=45)
    survived = live(pid)
    shutdown_delivery = {token:delivery(token) for token in (['FIRST_OK','BUSY_OK'] if variant == 'busy' else ['FIRST_OK'])}
    print(json.dumps({'phase':'shutdown', 'same_pid_alive':survived, 'expected':expected, 'delivery':shutdown_delivery}), flush=True)
    if expected == 'stop':
        assert not survived, 'stock daemon should kill its managed job'
    daemon = launch(2)
    wait(lambda: any(a.get('running') for a in ipc('actor_list', group_id=group)['actors']), 'restored actor')
    after = json.loads(receipt.read_text())
    if expected == 'detach':
        assert survived and live(pid), 'same worker must survive shutdown and restart'
        assert state()['daemonShort'] == job and state()['sessionId'] == session
        assert after['provider_session_id'] == session and after.get('captured_from') == 'claude_agent_view_resume', after
        if variant == 'busy': wait(lambda: reply_seen('BUSY_OK'), 'surviving busy reply')
        snapshot('before-follow-up')
        ipc('send', group_id=group, by='user', to=['worker'], text='Reply with exactly SECOND_OK. Do not use tools.', message_mode='send')
        wait(lambda: reply_seen('SECOND_OK'), 'follow-up tiny reply')
        assert live(pid), 'original worker died during the follow-up'
        assert state()['daemonShort'] == job and state()['sessionId'] == session
        restored_delivery = {token:delivery(token) for token in [*shutdown_delivery, 'SECOND_OK']}
        for result in restored_delivery.values():
            assert result['source_retained'] and result['states'][-1] in ('accepted', 'ambiguous'), result
            terminal = next(i for i, value in enumerate(result['states']) if value in ('accepted','ambiguous'))
            assert 'claimed' not in result['states'][terminal+1:], 'automatic redelivery after terminal outcome'
        (evidence/'delivery.json').write_text(json.dumps({'shutdown':shutdown_delivery,'restored':restored_delivery}, indent=2)+'\n')
        print(json.dumps({'phase':'restored', 'same_pid_alive':live(pid), 'same_job':job, 'same_session_id':session, 'follow_up_reply':True, 'busy_reply':reply_seen('BUSY_OK') if variant == 'busy' else None, 'delivery':restored_delivery}), flush=True)
    else:
        print(json.dumps({'phase':'stock_restart', 'original_pid_alive':live(pid), 'session_id':after['provider_session_id']}), flush=True)
    print('PASS', flush=True)
finally:
    try: snapshot('final-state')
    except Exception as error: print('diagnostic snapshot failed: '+str(error), flush=True)
    # Address only jobs in this disposable provider config, then stop its supervisor.
    for path in config.glob('jobs/*/state.json'):
        try:
            subprocess.run(['claude','stop',path.parent.name], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=20)
        except (OSError, subprocess.TimeoutExpired): pass
    if daemon and daemon.poll() is None:
        try:
            daemon.send_signal(signal.SIGTERM)
            daemon.wait(timeout=30)
        except subprocess.TimeoutExpired: daemon.kill(); daemon.wait()
        except ProcessLookupError: pass
    def owned_processes():
        owned = []
        for proc in pathlib.Path('/proc').iterdir():
            if not proc.name.isdigit(): continue
            try:
                environment = (proc/'environ').read_bytes().split(b'\0')
                if f'CLAUDE_CONFIG_DIR={config}'.encode() in environment or f'CCCC_HOME={home}'.encode() in environment:
                    owned.append(int(proc.name))
            except (OSError, PermissionError): pass
        return owned
    for pid in owned_processes():
        try: os.kill(pid, signal.SIGTERM)
        except ProcessLookupError: pass
    deadline = time.monotonic()+3
    while owned_processes() and time.monotonic()<deadline: time.sleep(.1)
    for pid in owned_processes():
        try: os.kill(pid, signal.SIGKILL)
        except ProcessLookupError: pass
    shutil.rmtree(root)
    shutil.rmtree(control_dir, ignore_errors=True)
PY
