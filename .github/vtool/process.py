"""Bounded noninteractive subprocesses; never resolve GitHub's CLI through PATH."""
import os
from pathlib import Path
import signal
import subprocess

IDENTITY = '212684707+chriscoveries@users.noreply.github.com'

def execute(*args, cwd=None, env=None, data=None, timeout=120, check=True, binary=False):
    program = Path(args[0]).name
    options = dict(os.environ)
    options.update(env or {})
    for key in ('GIT_CONFIG_COUNT','GIT_CONFIG_PARAMETERS','GIT_DIR','GIT_WORK_TREE','GIT_INDEX_FILE','GIT_OBJECT_DIRECTORY','GIT_ALTERNATE_OBJECT_DIRECTORIES','GIT_EXEC_PATH'):
        options.pop(key,None)
    options.update(GIT_TERMINAL_PROMPT='0', GH_PROMPT_DISABLED='1', GH_HOST='github.com',
                   GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL='/dev/null',
                   GIT_ATTR_NOSYSTEM='1', GIT_NO_REPLACE_OBJECTS='1')
    if program == 'git':
        args = ('/usr/bin/git', '-c', 'credential.helper=', '-c',
                'credential.helper=!/usr/bin/gh auth git-credential', *args[1:])
    elif program == 'gh':
        args = ('/usr/bin/gh', *args[1:])
    if timeout <= 0:
        raise ValueError('positive subprocess timeout required')
    p = subprocess.Popen(args, cwd=cwd, env=options, stdin=subprocess.PIPE if data is not None else subprocess.DEVNULL,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=not binary, start_new_session=True)
    try:
        stdout, stderr = p.communicate(data, timeout=timeout)
    except BaseException as error:
        try: os.killpg(p.pid, signal.SIGKILL)
        except ProcessLookupError: pass
        p.wait(timeout=10)
        if isinstance(error,subprocess.TimeoutExpired): raise TimeoutError(f'{args[0]} exceeded {timeout}s') from None
        raise
    result = subprocess.CompletedProcess(args, p.returncode, stdout, stderr)
    if check and p.returncode:
        # Do not print argv or environment: either may carry authentication.
        message = stderr.decode(errors='replace') if binary else stderr
        raise RuntimeError(f'{args[0]} exited {p.returncode}: {message.strip()}')
    return result

def run(*args, **kw):
    return execute(*args, **kw).stdout.strip()
