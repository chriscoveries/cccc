#!/usr/bin/env python3
"""Create verified, non-destructive zstd sidecars for explicitly sealed ledgers.

This helper is disabled until an administrator supplies a root-owned manifest
and installs a service with the resource contract checked below. It never
removes or replaces a source, a staging directory, or an existing bundle.
"""
import argparse
import ctypes
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import secrets
import selectors
import stat
import subprocess
import sys
import time
from typing import Literal

from pydantic import BaseModel, ConfigDict, Field

CHUNK = 1024 * 1024
DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
FILE_FLAGS = os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC
ZSTD = '/usr/bin/zstd'
LOW_PRIORITY = ['/usr/bin/ionice', '-c', '3', '/usr/bin/nice', '-n', '19']


class StrictModel(BaseModel):
    model_config = ConfigDict(extra='forbid', strict=True)


class Limits(StrictModel):
    max_input_bytes: int = Field(gt=0)
    max_output_bytes: int = Field(gt=0)
    max_runtime_seconds: int = Field(gt=0)
    max_memory_bytes: int = Field(gt=0)
    min_free_bytes: int = Field(gt=0)


class Candidate(StrictModel):
    path: str
    owner: str = Field(min_length=1, max_length=128)
    sealing_contract: str = Field(min_length=1, max_length=512)
    device: int = Field(ge=0)
    inode: int = Field(gt=0)
    size: int = Field(ge=0)
    mtime_ns: int = Field(ge=0)
    sha256: str = Field(pattern=r'^[0-9a-f]{64}$')


class Config(StrictModel):
    version: Literal[1]
    service: str = Field(pattern=r'^[A-Za-z0-9_.-]+\.service$')
    source_root: str
    archive_root: str
    limits: Limits
    candidates: list[Candidate] = Field(max_length=10000)


class Receipt(StrictModel):
    version: Literal[1]
    source: Candidate
    archive_sha256: str = Field(pattern=r'^[0-9a-f]{64}$')
    archive_size: int = Field(ge=0)
    zstd_version: str = Field(min_length=1, max_length=256)
    level: Literal[11]
    integrity: Literal['verified']
    disposition: Literal['original-retained']
    limits: Limits
    reservation_bytes: int = Field(gt=0)
    service: str
    runtime_max_usec: int = Field(gt=0)
    memory_max_bytes: int = Field(gt=0)
    published_at_unix: int = Field(gt=0)


def strict_json(data):
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError('duplicate JSON key')
            result[key] = value
        return result
    def constant(_):
        raise ValueError('non-finite JSON number')
    return json.loads(data, object_pairs_hook=pairs, parse_constant=constant)


def read_limited(fd, limit):
    result = bytearray()
    while chunk := os.read(fd, min(CHUNK, limit + 1 - len(result))):
        result.extend(chunk)
        if len(result) > limit:
            raise ValueError('input exceeds limit')
    return bytes(result)


def open_absolute(path, *, directory):
    parsed = Path(path)
    if not parsed.is_absolute() or '..' in parsed.parts:
        raise ValueError('absolute normalized path required')
    fd = os.open('/', DIR_FLAGS)
    try:
        parts = parsed.parts[1:]
        for index, component in enumerate(parts):
            flags = DIR_FLAGS if directory or index < len(parts) - 1 else FILE_FLAGS
            child = os.open(component, flags, dir_fd=fd)
            os.close(fd)
            fd = child
        return fd
    except BaseException:
        os.close(fd)
        raise


def load_config(path):
    # Root ownership of the file alone is insufficient if an ancestor can be
    # replaced. Each directory is checked through the descriptor used to open
    # its child; symlinks and writable ancestors are refused.
    parsed = Path(path)
    if not parsed.is_absolute() or '..' in parsed.parts:
        raise ValueError('invalid configuration path')
    fd = os.open('/', DIR_FLAGS)
    try:
        for component in parsed.parts[1:-1]:
            info = os.fstat(fd)
            if info.st_uid != 0 or info.st_mode & 0o022:
                raise ValueError('untrusted configuration ancestor')
            child = os.open(component, DIR_FLAGS, dir_fd=fd)
            os.close(fd)
            fd = child
        info = os.fstat(fd)
        if info.st_uid != 0 or info.st_mode & 0o022:
            raise ValueError('untrusted configuration directory')
        conf = os.open(parsed.name, FILE_FLAGS, dir_fd=fd)
        try:
            info = os.fstat(conf)
            if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022 or info.st_nlink != 1:
                raise ValueError('untrusted configuration file')
            return Config.model_validate(strict_json(read_limited(conf, CHUNK)))
        finally:
            os.close(conf)
    finally:
        os.close(fd)


def open_archive_root(path):
    parsed = Path(path)
    if not parsed.is_absolute() or '..' in parsed.parts or not parsed.parts[1:]:
        raise ValueError('invalid archive root')
    fd = os.open('/', DIR_FLAGS)
    try:
        for component in parsed.parts[1:]:
            info = os.fstat(fd)
            if info.st_uid != 0 or info.st_mode & 0o022:
                raise ValueError('untrusted archive ancestor')
            child = os.open(component, DIR_FLAGS, dir_fd=fd)
            os.close(fd)
            fd = child
        info = os.fstat(fd)
        if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
            raise ValueError('archive root must be private to the service user')
        return fd
    except BaseException:
        os.close(fd)
        raise


def systemd_contract(config):
    # RuntimeMaxSec applies to Type=exec; do not silently accept oneshot.
    raw = subprocess.run(['/usr/bin/systemctl', 'show', config.service, '--no-pager',
                          '-p', 'Id', '-p', 'Type', '-p', 'MainPID', '-p', 'ControlGroup'],
                         check=True, capture_output=True, text=True, timeout=10).stdout
    properties = dict(line.split('=', 1) for line in raw.splitlines() if '=' in line)
    if properties.get('Id') != config.service or properties.get('Type') != 'exec':
        raise ValueError('service type or identity differs')
    if properties.get('MainPID') != str(os.getpid()):
        raise ValueError('not running as the service main process')
    cgroup = properties.get('ControlGroup', '')
    if not cgroup.startswith('/') or '..' in Path(cgroup).parts:
        raise ValueError('invalid service control group')
    membership = Path('/proc/self/cgroup').read_text().splitlines()
    if '0::' + cgroup not in membership:
        raise ValueError('service control group differs')
    encoded = ''.join(ch if ch.isascii() and ch.isalnum() else f'_{ord(ch):02x}' for ch in config.service)
    object_path = '/org/freedesktop/systemd1/unit/' + encoded
    result = subprocess.run(['/usr/bin/busctl', 'get-property', 'org.freedesktop.systemd1', object_path,
                             'org.freedesktop.systemd1.Service', 'RuntimeMaxUSec'],
                            check=True, capture_output=True, text=True, timeout=10).stdout.strip().split()
    if len(result) != 2 or result[0] != 't' or not result[1].isdigit():
        raise ValueError('runtime limit is not observable')
    runtime = int(result[1])
    if not 0 < runtime <= config.limits.max_runtime_seconds * 1_000_000:
        raise ValueError('service runtime limit exceeds manifest')
    base = Path('/sys/fs/cgroup') / cgroup.lstrip('/')
    memory_text = (base / 'memory.max').read_text().strip()
    if not memory_text.isdigit() or not 0 < int(memory_text) <= config.limits.max_memory_bytes:
        raise ValueError('service memory limit is not enforced')
    quota = (base / 'cpu.max').read_text().strip().split()
    if len(quota) != 2 or not all(item.isdigit() and int(item) > 0 for item in quota) or int(quota[0]) > int(quota[1]):
        raise ValueError('service CPU quota is not enforced')
    if os.getpriority(os.PRIO_PROCESS, 0) < 19:
        raise ValueError('service scheduling priority differs')
    priority = subprocess.run(['/usr/bin/ionice', '-p', str(os.getpid())], check=True,
                              capture_output=True, text=True, timeout=10).stdout.strip()
    if priority != 'idle':
        raise ValueError('service I/O priority differs')
    return runtime, int(memory_text)


def source_open(config, candidate):
    root, path = Path(config.source_root), Path(candidate.path)
    if not root.is_absolute() or not path.is_absolute() or '..' in path.parts or not path.is_relative_to(root):
        raise ValueError('source is outside the approved root')
    relative = path.relative_to(root)
    parts = relative.parts
    if (len(parts) != 5 or not re.fullmatch(r'g_[a-z0-9]+', parts[0])
            or parts[1:4] != ('state', 'ledger', 'segments')
            or not re.fullmatch(r'ledger\.[0-9]{8}T[0-9]{6}Z\.[0-9]{6}\.jsonl', parts[4])):
        raise ValueError('source is not an approved sealed ledger segment')
    if candidate.size > config.limits.max_input_bytes:
        raise ValueError('source exceeds manifest limit')
    fd = open_absolute(str(root), directory=True)
    try:
        for part in parts[:-1]:
            child = os.open(part, DIR_FLAGS, dir_fd=fd)
            os.close(fd)
            fd = child
        source = os.open(parts[-1], FILE_FLAGS, dir_fd=fd)
        return fd, source
    except BaseException:
        os.close(fd)
        raise


def source_check(fd, candidate):
    before = os.fstat(fd)
    if (not stat.S_ISREG(before.st_mode) or before.st_nlink != 1
            or (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns)
            != (candidate.device, candidate.inode, candidate.size, candidate.mtime_ns)):
        raise ValueError('sealed source identity differs')
    os.lseek(fd, 0, os.SEEK_SET)
    digest, count = hashlib.sha256(), 0
    while chunk := os.read(fd, CHUNK):
        count += len(chunk)
        if count > candidate.size:
            raise ValueError('sealed source grew')
        digest.update(chunk)
    after = os.fstat(fd)
    if (count != candidate.size or digest.hexdigest() != candidate.sha256
            or (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns)
            != (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns)):
        raise ValueError('sealed source changed')
    os.lseek(fd, 0, os.SEEK_SET)
    return after


def source_recheck_path(config, candidate, held_dir, held_source):
    # Reopening only for comparison detects replacement of any path ancestor;
    # all data reads and publication remain bound to the held descriptors.
    new_dir, new_source = source_open(config, candidate)
    try:
        for left, right in ((held_dir, new_dir), (held_source, new_source)):
            a, b = os.fstat(left), os.fstat(right)
            if (a.st_dev, a.st_ino) != (b.st_dev, b.st_ino):
                raise ValueError('source path changed')
    finally:
        os.close(new_source)
        os.close(new_dir)


def run_bounded(argv, deadline, *, stdin=None, stdout=None, output_limit=None):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError('runtime limit reached')
    def restrict():
        if output_limit is not None:
            resource.setrlimit(resource.RLIMIT_FSIZE, (output_limit, output_limit))
    return subprocess.run(LOW_PRIORITY + argv, stdin=stdin, stdout=stdout, stderr=subprocess.DEVNULL,
                          timeout=remaining, check=True, preexec_fn=restrict)


def decoded_digest(fd, deadline, limit):
    # Pass the already-open file as stdin. The /proc fd path is a symlink and
    # zstd may refuse it; reopening a pathname would also lose the binding.
    proc = subprocess.Popen(LOW_PRIORITY + [ZSTD, '-d', '-c', '-q'], stdin=fd,
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    digest, total = hashlib.sha256(), 0
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(proc.stdout, selectors.EVENT_READ)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    raise TimeoutError('decompression timed out')
                chunk = os.read(proc.stdout.fileno(), CHUNK)
                if not chunk:
                    break
                total += len(chunk)
                if total > limit:
                    raise ValueError('decoded source exceeds limit')
                digest.update(chunk)
        if proc.wait(timeout=max(0.01, deadline - time.monotonic())) != 0:
            raise ValueError('zstd decode failed')
        return total, digest.hexdigest()
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
        proc.stdout.close()


def file_digest(fd, limit):
    os.lseek(fd, 0, os.SEEK_SET)
    digest, total = hashlib.sha256(), 0
    while chunk := os.read(fd, CHUNK):
        total += len(chunk)
        if total > limit:
            raise ValueError('archive exceeds limit')
        digest.update(chunk)
    return total, digest.hexdigest()


def rename_noreplace(parent, old, new):
    libc = ctypes.CDLL(None, use_errno=True)
    if not hasattr(libc, 'renameat2'):
        raise OSError('atomic no-replace directory rename unavailable')
    libc.renameat2.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
    libc.renameat2.restype = ctypes.c_int
    result = libc.renameat2(parent, os.fsencode(old), parent, os.fsencode(new), 1)
    if result != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))


def publish(config, candidate, root_fd, runtime, memory, deadline):
    source_dir, source = source_open(config, candidate)
    stage_fd = archive = None
    stage = '.staging-' + secrets.token_hex(16)
    try:
        before = source_check(source, candidate)
        os.mkdir(stage, 0o700, dir_fd=root_fd)
        stage_fd = os.open(stage, DIR_FLAGS, dir_fd=root_fd)
        archive = os.open('archive.zst', os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                          0o600, dir_fd=stage_fd)
        os.posix_fallocate(archive, 0, config.limits.max_output_bytes)
        free = os.fstatvfs(root_fd)
        if free.f_bavail * free.f_frsize < config.limits.min_free_bytes:
            raise ValueError('insufficient free space after reservation')
        run_bounded([ZSTD, '--single-thread', '-11', '-q', '-c'], deadline, stdin=source, stdout=archive,
                    output_limit=config.limits.max_output_bytes)
        written = os.lseek(archive, 0, os.SEEK_CUR)
        if written > config.limits.max_output_bytes:
            raise ValueError('compressed archive exceeds limit')
        os.ftruncate(archive, written)
        os.fsync(archive)
        os.lseek(archive, 0, os.SEEK_SET)
        run_bounded([ZSTD, '-t', '-q'], deadline, stdin=archive)
        os.lseek(archive, 0, os.SEEK_SET)
        if decoded_digest(archive, deadline, candidate.size) != (candidate.size, candidate.sha256):
            raise ValueError('archive contents differ from source')
        current = source_check(source, candidate)
        entry = os.stat(Path(candidate.path).name, dir_fd=source_dir, follow_symlinks=False)
        if ((entry.st_dev, entry.st_ino) != (before.st_dev, before.st_ino)
                or (current.st_ctime_ns, current.st_mtime_ns) != (before.st_ctime_ns, before.st_mtime_ns)):
            raise ValueError('source changed before publication')
        source_recheck_path(config, candidate, source_dir, source)
        size, digest = file_digest(archive, config.limits.max_output_bytes)
        version = subprocess.run([ZSTD, '--version'], check=True, capture_output=True,
                                 text=True, timeout=min(10, max(0.01, deadline - time.monotonic()))).stdout.strip()
        receipt = Receipt(version=1, source=candidate, archive_sha256=digest, archive_size=size,
                          zstd_version=version, level=11, integrity='verified',
                          disposition='original-retained', limits=config.limits,
                          reservation_bytes=config.limits.max_output_bytes, service=config.service,
                          runtime_max_usec=runtime, memory_max_bytes=memory, published_at_unix=int(time.time()))
        fd = os.open('receipt.json', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=stage_fd)
        with os.fdopen(fd, 'wb') as stream:
            stream.write(receipt.model_dump_json(indent=2).encode() + b'\n')
            stream.flush()
            os.fsync(stream.fileno())
        os.fsync(stage_fd)
        latest = os.fstat(source)
        if ((latest.st_dev, latest.st_ino, latest.st_size, latest.st_mtime_ns, latest.st_ctime_ns)
                != (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns)):
            raise ValueError('source changed before publication')
        source_recheck_path(config, candidate, source_dir, source)
        final = f'bundle-{candidate.sha256}-{candidate.device:x}-{candidate.inode:x}'
        rename_noreplace(root_fd, stage, final)
        os.fsync(root_fd)
        return final
    finally:
        if archive is not None:
            os.close(archive)
        if stage_fd is not None:
            os.close(stage_fd)
        os.close(source)
        os.close(source_dir)


def bundle_name(candidate):
    return f'bundle-{candidate.sha256}-{candidate.device:x}-{candidate.inode:x}'


def verify_existing(config, candidate, root_fd, name, deadline):
    if name != bundle_name(candidate):
        raise ValueError('bundle name differs from candidate')
    bundle = os.open(name, DIR_FLAGS, dir_fd=root_fd)
    archive = source_dir = source = None
    try:
        info = os.fstat(bundle)
        if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
            raise ValueError('untrusted bundle directory')
        if set(os.listdir(bundle)) != {'archive.zst', 'receipt.json'}:
            raise ValueError('incomplete bundle')
        receipt_fd = os.open('receipt.json', FILE_FLAGS, dir_fd=bundle)
        try:
            info = os.fstat(receipt_fd)
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1 or info.st_mode & 0o077:
                raise ValueError('untrusted receipt')
            receipt = Receipt.model_validate(strict_json(read_limited(receipt_fd, CHUNK)))
        finally:
            os.close(receipt_fd)
        if receipt.source != candidate or receipt.archive_size > config.limits.max_output_bytes:
            raise ValueError('receipt does not bind candidate')
        archive = os.open('archive.zst', FILE_FLAGS, dir_fd=bundle)
        info = os.fstat(archive)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1 or info.st_mode & 0o077:
            raise ValueError('untrusted archive')
        if file_digest(archive, config.limits.max_output_bytes) != (receipt.archive_size, receipt.archive_sha256):
            raise ValueError('archive checksum differs')
        os.lseek(archive, 0, os.SEEK_SET)
        run_bounded([ZSTD, '-t', '-q'], deadline, stdin=archive)
        os.lseek(archive, 0, os.SEEK_SET)
        if decoded_digest(archive, deadline, candidate.size) != (candidate.size, candidate.sha256):
            raise ValueError('archive content differs')
        source_dir, source = source_open(config, candidate)
        source_check(source, candidate)
        source_recheck_path(config, candidate, source_dir, source)
    finally:
        if source is not None:
            os.close(source)
        if source_dir is not None:
            os.close(source_dir)
        if archive is not None:
            os.close(archive)
        os.close(bundle)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', default='/etc/cccc/history-zstd.json')
    config = load_config(parser.parse_args().config)
    runtime, memory = systemd_contract(config)
    root = open_archive_root(config.archive_root)
    try:
        fcntl.flock(root, fcntl.LOCK_EX | fcntl.LOCK_NB)
        deadline = time.monotonic() + config.limits.max_runtime_seconds
        candidates = {bundle_name(candidate): candidate for candidate in config.candidates}
        if len(candidates) != len(config.candidates):
            raise ValueError('duplicate manifest candidate')
        entries = os.listdir(root)
        if len(entries) > 10000:
            raise ValueError('archive root exceeds entry limit')
        for entry in entries:
            if entry not in candidates:
                # Includes interrupted staging and final bundles whose source
                # is not in the currently reviewed manifest. Never delete.
                raise ValueError('archive root requires reconciliation')
            verify_existing(config, candidates[entry], root, entry, deadline)
        for candidate in config.candidates:
            name = bundle_name(candidate)
            if name in entries:
                print(name + ' already verified', flush=True)
            else:
                print(publish(config, candidate, root, runtime, memory, deadline), flush=True)
        return 0
    finally:
        os.close(root)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, ValueError, TimeoutError, subprocess.SubprocessError) as error:
        print(f'history archive held: {type(error).__name__}', file=sys.stderr)
        sys.exit(1)
