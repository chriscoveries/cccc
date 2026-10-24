"""Runner-only deterministic integration; contains no deployment or host credentials."""
import contextlib, hashlib, json, os
from pathlib import Path
import tempfile
from process import run, execute
IDENTITY = '212684707+chriscoveries@users.noreply.github.com'
GIT_ENV = dict(os.environ)
def digest(b): return hashlib.sha256(b).hexdigest()
def canonical(v): return json.dumps(v,sort_keys=True,separators=(',',':')).encode()
def require_runner():
    if os.environ.get('GITHUB_ACTIONS') != 'true' or os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted':
        raise ValueError('integration requires a GitHub-hosted Actions runner')


def git(repo, *args, **kw):
    return run('git', '-C', str(repo), *args, **kw)

def merge_commit(repo, left, right, tree, ref):
    epoch = max(int(git(repo, 'show', '-s', '--format=%ct', sha)) for sha in (left, right)) + 1
    env = dict(GIT_ENV, GIT_AUTHOR_NAME='chriscoveries', GIT_COMMITTER_NAME='chriscoveries',
               GIT_AUTHOR_EMAIL=IDENTITY, GIT_COMMITTER_EMAIL=IDENTITY,
               GIT_AUTHOR_DATE=f'@{epoch} +0000', GIT_COMMITTER_DATE=f'@{epoch} +0000')
    return git(repo, '-c', 'commit.gpgSign=false', 'commit-tree', tree, '-p', left, '-p', right,
               '-m', 'Merge ' + ref, env=env)

def upstream_matches(repo, base, patch):
    ancestor = execute('git', '-C', str(repo), 'merge-base', '--is-ancestor', patch, base, check=False).returncode
    if ancestor == 0:
        return [patch]
    cherry = git(repo, 'cherry', base, patch).splitlines()
    if not cherry or any(line.startswith('+') for line in cherry):
        return []
    common = git(repo, 'merge-base', base, patch)
    def ids(revision):
        changes = git(repo, 'log', '--no-merges', '--pretty=medium', '-p', revision)
        return dict(line.split() for line in run('git', 'patch-id', '--stable', data=changes).splitlines())
    upstream = ids(common + '..' + base)
    source = ids(common + '..' + patch)
    if not source or not source.keys() <= upstream.keys(): return []
    merged=execute('git','-C',str(repo),'merge-tree','--write-tree',base,patch,check=False)
    if merged.returncode or merged.stdout.splitlines()[0] != git(repo,'rev-parse',base+'^{tree}'): return []
    return [upstream[k] for k in source]


@contextlib.contextmanager
def prepared(resolved):
    require_runner()
    with tempfile.TemporaryDirectory(prefix='vtool-') as td:
        repo = Path(td) / 'git'
        run('git', 'init', '--bare', '-q', str(repo))
        git(repo, 'config', 'core.hooksPath', '/dev/null')
        git(repo, 'fetch', '--quiet', '--no-tags', 'https://github.com/ChesterRa/cccc.git', resolved['base'])
        for patch in resolved['patches']:
            git(repo, 'fetch', '--quiet', '--no-tags', 'https://github.com/chriscoveries/cccc.git', patch['sha'])
        yield repo

def plan_in(repo, resolved):
    plan = json.loads(json.dumps(resolved))
    plan['lockhash'] = digest(canonical(resolved))
    current = plan['base']
    for patch in plan['patches']:
        matches = upstream_matches(repo, plan['base'], patch['sha'])
        if matches:
            patch.update(status='already_in_base', upstream_commits=matches)
            continue
        merged = execute('git', '-C', str(repo), '-c', 'merge.renames=true',
                         'merge-tree', '--write-tree', '--name-only', current, patch['sha'], check=False)
        lines = merged.stdout.splitlines()
        if merged.returncode == 1:
            files = lines[1:lines.index('')] if '' in lines else lines[1:]
            patch.update(status='conflicts', files=files)
        elif merged.returncode:
            raise RuntimeError('merge-tree: ' + merged.stderr)
        else:
            current = merge_commit(repo, current, patch['sha'], lines[0], patch['ref'])
            patch.update(status='applies', merge_sha=current, tree=lines[0])
    plan['integration_sha'] = current
    plan['branch'] = 'vt/' + resolved['tag'] + '-' + plan['lockhash'][:8]
    plan['ready'] = all(p['status'] != 'conflicts' for p in plan['patches'])
    return plan


def worker(request):
    require_runner()
    plan = dict(request)
    operation = plan.pop('operation')
    if plan.pop('controller_sha',None) != os.environ.get('GITHUB_SHA'): raise ValueError('controller head changed')
    if operation not in ('plan','build') or plan.get('schema') != 1:
        raise ValueError('unknown integration request')
    if set(plan) != {'schema','manifest_sha256','recipe','tag','base','patches','cwf_sha'}:
        raise ValueError('unexpected integration inputs')
    import re, uuid
    if not re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+',plan['tag']):
        raise ValueError('release tag required')
    for sha in [plan['base'],plan['cwf_sha'],plan['manifest_sha256']]:
        if not re.fullmatch('[0-9a-f]{40}|[0-9a-f]{64}',sha): raise ValueError('invalid immutable input')
    for patch in plan['patches']:
        if set(patch) != {'ref','sha'} or not re.fullmatch('[0-9a-f]{40}',patch['sha']): raise ValueError('invalid patch')
        run('git','check-ref-format','refs/heads/'+patch['ref'])
        if patch['ref']=='feat/actor-model-v2': raise ValueError('protected branch')
    plan['recipe'] = {'git':run('git','--version'),'merge':'merge-tree/ort;renames=true;commit-tree;parent-max-epoch+1','identity':IDENTITY}
    with prepared(plan) as repo:
        plan = plan_in(repo,plan)
        if operation=='build' and plan['ready']:
            sha,branch=plan['integration_sha'],plan['branch']
            # Verify every newly introduced author and committer before publishing.
            entries=git(repo,'log','--format=%an <%ae> | %cn <%ce>',plan['base']+'..'+sha).splitlines()
            expected='chriscoveries <'+IDENTITY+'> | chriscoveries <'+IDENTITY+'>'
            if any(e!=expected for e in entries): raise ValueError('non-public commit identity')
            url='https://github.com/chriscoveries/cccc.git'
            old=run('git','ls-remote',url,'refs/heads/'+branch)
            if old and old.split()[0]!=sha: raise ValueError('immutable branch already differs')
            if not old:
                staging='refs/heads/vt/upload-'+uuid.uuid4().hex
                try:
                    git(repo,'push',url,sha+':'+staging)
                    run('gh','api','-X','POST','repos/chriscoveries/cccc/git/refs','-f','ref=refs/heads/'+branch,'-f','sha='+sha)
                finally:
                    run('gh','api','-X','DELETE','repos/chriscoveries/cccc/git/'+staging.removeprefix('refs/'))
    return plan


if __name__ == '__main__':
    Path('plan.json').write_text(json.dumps(worker(json.loads(os.environ['VTOOL_REQUEST'])),indent=2)+'\n')
