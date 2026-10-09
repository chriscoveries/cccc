#!/usr/bin/env bash
# Real Claude/CCCC shadow reproduction. No service or shared session directories.
# Usage: CCCC_REPRO_BINARY=/absolute/path/to/cccc ./opus-retarget-repro.sh
# Authentication: inherited provider env, or credentials/account from CCCC_REPRO_AUTH_DIR.
set -euo pipefail
python3 - <<'PY'
import hashlib, json, os, pathlib, shutil, signal, subprocess, tempfile, time, uuid
binary = pathlib.Path(os.environ.get('CCCC_REPRO_BINARY', str(pathlib.Path.cwd() / 'target/debug/cccc'))).resolve()
evidence = pathlib.Path(os.environ.get('CCCC_REPRO_EVIDENCE', str(pathlib.Path.cwd() / 'target/resume-session-evidence')))
evidence.mkdir(parents=True, exist_ok=True)
scratch = tempfile.TemporaryDirectory(prefix='cccc-resume-repro-')
root = pathlib.Path(scratch.name)
config = root / 'claude-config'
workspace = root / 'workspace'
config.mkdir(); workspace.mkdir(); (root / 'provider-home').mkdir(); (root / 'bin').mkdir()
env = os.environ.copy()
env.update(CCCC_HOME=str(root / 'cccc-home'), CLAUDE_CONFIG_DIR=str(config),
           HOME=str(root / 'provider-home'), CCCC_LAUNCHER_PATH=str(binary),
           CCCC_RUNTIME_RESUME='1', CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC='1')
env.pop('CCCC_GROUP_ID', None); env.pop('CCCC_ACTOR_ID', None)
probe = subprocess.run([str(binary), 'actor', 'resume-session', '--help'], env=env, capture_output=True, text=True)
if probe.returncode:
    (evidence / 'stock-cli-absent.log').write_text(probe.stdout + probe.stderr)
    print('STOCK: actor resume-session is absent (exit %d)' % probe.returncode)
    shutil.rmtree(root)
    raise SystemExit(0 if 'unrecognized subcommand' in probe.stderr else 1)
# Use the underlying direct executable; host shell wrappers may force a shared config.
real = pathlib.Path(os.environ.get('CCCC_REPRO_CLAUDE', shutil.which('claude.real') or shutil.which('claude') or 'claude')).resolve()
wrapper = root / 'bin' / 'claude'
wrapper.write_text('#!/usr/bin/env python3\nimport json, os, pathlib, sys\n'
                   + 'with pathlib.Path(' + repr(str(config / 'launch-arguments.jsonl')) + ').open("a") as log: log.write(json.dumps(sys.argv[1:]) + "\\n")\n'
                   + 'os.execv(' + repr(str(real)) + ', [' + repr(str(real)) + '] + sys.argv[1:])\n')
wrapper.chmod(0o700)
auth = pathlib.Path(os.environ.get('CCCC_REPRO_AUTH_DIR', str(pathlib.Path.home() / '.claude')))
if (auth / '.credentials.json').is_file():
    shutil.copyfile(auth / '.credentials.json', config / '.credentials.json')
    (config / '.credentials.json').chmod(0o600)
account = {}
if (auth / '.claude.json').is_file():
    source = json.loads((auth / '.claude.json').read_text())
    if 'oauthAccount' in source: account['oauthAccount'] = source['oauthAccount']
account.update(hasCompletedOnboarding=True, bypassPermissionsModeAccepted=True,
               projects={str(workspace): {'hasTrustDialogAccepted': True, 'allowedTools': []}})
(config / '.claude.json').write_text(json.dumps(account))
(config / 'settings.json').write_text(json.dumps({'skipDangerousModePermissionPrompt': True, 'autoUpdates': False}))
daemon = None
group = None
log = (evidence / 'real-shadow.log').open('w')

def run(*args):
    result = subprocess.run([str(binary), *args], cwd=workspace, env=env, capture_output=True, text=True, timeout=90)
    log.write('cccc ' + ' '.join(args) + '\n' + result.stdout + result.stderr); log.flush()
    if result.returncode: raise RuntimeError(result.stderr)
    return json.loads(result.stdout) if result.stdout.strip().startswith('{') else result.stdout

def wait(check, description):
    until = time.monotonic() + 90
    while time.monotonic() < until:
        value = check()
        if value: return value
        time.sleep(.25)
    raise RuntimeError('Timeout: ' + description)

def transcript(session):
    matches = list((config / 'projects').glob('*/' + session + '.jsonl'))
    return matches[0] if matches else None

def answers(session):
    path = transcript(session)
    if not path: return []
    values = []
    for line in path.read_text().splitlines():
        try: value = json.loads(line)
        except json.JSONDecodeError: continue
        if value.get('type') == 'assistant':
            content = value.get('message', {}).get('content', [])
            values.extend(block.get('text', '') for block in content if isinstance(block, dict) and block.get('type') == 'text')
    return values

try:
    daemon = subprocess.Popen([str(binary), 'daemon', 'run'], cwd=workspace, env=env, stdout=log, stderr=log, start_new_session=True)
    wait(lambda: (root / 'cccc-home' / 'daemon' / 'ccccd.sock').exists(), 'scratch daemon socket')
    created = run('group', 'create', '--title', 'Resume session reproduction')
    group = created['group']['group_id']
    env['CCCC_GROUP_ID'] = group
    run('attach', str(workspace), '--group', group)
    run('actor', 'add', 'claude-1', '--runtime', 'claude', '--command', str(root / 'bin' / 'claude') + ' --model haiku', '--env', 'CLAUDE_CONFIG_DIR=' + str(config), '--env', 'HOME=' + str(root / 'provider-home'))
    run('group', 'start', '--group', group)
    run('actor', 'start', 'claude-1')
    receipt = root / 'cccc-home' / 'groups' / group / 'state' / 'runtime_sessions' / 'claude-1.json'
    a = json.loads(receipt.read_text())['provider_session_id']
    token = 'remember-' + uuid.uuid4().hex[:12]
    run('send', 'Remember this token for later: ' + token + '. Reply only ACK. Do not use tools.', '--to', 'claude-1')
    wait(lambda: any('ACK' in answer for answer in answers(a)), 'A acknowledgement')
    run('actor', 'stop', 'claude-1')
    saved_a = json.loads(receipt.read_text())
    # Scratch-only fault injection: retire the receipt so Start creates throwaway B.
    receipt.unlink()
    run('actor', 'start', 'claude-1')
    b = json.loads(receipt.read_text())['provider_session_id']
    assert a != b
    run('send', 'Reply only READY. Do not use tools.', '--to', 'claude-1')
    wait(lambda: any('READY' in answer for answer in answers(b)), 'B acknowledgement')
    run('actor', 'stop', 'claude-1')
    wrong = json.loads(receipt.read_text())
    wrong.update(status='resume_failed', resume_eligible=False, last_resume_error='Claude copied the session because another process held it open')
    receipt.write_text(json.dumps(wrong))
    # Model-only actor drift is separate from restoring the exact saved conversation.
    run('actor', 'update', 'claude-1', '--command', str(wrapper) + ' --model opus')
    selection = run('actor', 'resume-session', 'claude-1', a)
    resumed = json.loads(receipt.read_text())
    assert resumed['provider_session_id'] == a
    model_application = resumed['model_application']
    assert model_application['configured_model'] == 'opus'
    assert model_application['configured_model_applied'] is False
    assert 'haiku' in model_application['retained_model']
    assert resumed['model'] == model_application['retained_model']
    assert selection['model_application'] == model_application
    launch_arguments = [json.loads(line) for line in (config / 'launch-arguments.jsonl').read_text().splitlines()]
    resumed_launches = [args for args in launch_arguments if '--bg' in args and '--resume' in args]
    assert resumed_launches and all(args == ['--bg', '--resume', a] for args in resumed_launches)
    (evidence / 'resume-launch-arguments.json').write_text(json.dumps(resumed_launches, indent=2))
    assert any(item['session_id'] == b for item in resumed['previous_sessions'])
    before = len(answers(a))
    run('send', 'Repeat the token I asked you to remember earlier. Reply only with that token. Do not use tools.', '--to', 'claude-1')
    reply = wait(lambda: next((answer for answer in answers(a)[before:] if token in answer), None), 'A remembers its earlier token')
    proof = {'session_a': a, 'session_b': b, 'token': token, 'reply': reply, 'receipt': resumed, 'claude_version': subprocess.check_output([str(real), '--version'], env=env, text=True).strip()}
    (evidence / 'real-shadow-proof.json').write_text(json.dumps(proof, indent=2))
    print('PASS: resumed A with its history; B archived in previous_sessions')
finally:
    if group:
        try: run('group', 'stop', '--group', group)
        except Exception as error: log.write('Cleanup group stop: ' + str(error) + '\n')
    if daemon:
        try: run('daemon', 'stop')
        except Exception: daemon.terminate()
        try: daemon.wait(timeout=20)
        except subprocess.TimeoutExpired: os.killpg(daemon.pid, signal.SIGKILL); daemon.wait()
    # Stop only processes whose environment names this disposable config/home.
    owned = []
    for proc in pathlib.Path('/proc').iterdir():
        if not proc.name.isdigit() or int(proc.name) == os.getpid(): continue
        try:
            variables = (proc / 'environ').read_bytes().split(b'\0')
            if ('CLAUDE_CONFIG_DIR=' + str(config)).encode() in variables or ('CCCC_HOME=' + str(root / 'cccc-home')).encode() in variables:
                owned.append(int(proc.name))
                os.kill(int(proc.name), signal.SIGTERM)
        except (OSError, PermissionError): pass
    until = time.monotonic() + 5
    while owned and time.monotonic() < until:
        owned = [pid for pid in owned if pathlib.Path('/proc', str(pid)).exists()]
        if owned: time.sleep(.1)
    for pid in owned:
        try:
            variables = pathlib.Path('/proc', str(pid), 'environ').read_bytes().split(b'\0')
            if ('CLAUDE_CONFIG_DIR=' + str(config)).encode() in variables or ('CCCC_HOME=' + str(root / 'cccc-home')).encode() in variables:
                os.kill(pid, signal.SIGKILL)
        except OSError: pass
    digest = hashlib.sha256(str(config.resolve()).encode()).hexdigest()[:8]
    shutil.rmtree(pathlib.Path('/tmp') / ('cc-daemon-' + str(os.getuid())) / digest, ignore_errors=True)
    log.close()
    shutil.rmtree(root)
PY
