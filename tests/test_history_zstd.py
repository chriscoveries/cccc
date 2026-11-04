"""Sidecar controls use privacy-redacted events captured from a real ledger."""
from __future__ import annotations

import hashlib
import os
from pathlib import Path
import sys
import time

import pytest

from scripts import history_zstd as archive


FIXTURE = (Path(__file__).parents[1] / "crates/cccc-core/tests/fixtures"
           / "ledger-zstd-redacted.jsonl")


@pytest.fixture
def candidate(tmp_path: Path):
    root = tmp_path / "groups"
    parent = root / "g_example" / "state" / "ledger" / "segments"
    parent.mkdir(parents=True)
    source = parent / "ledger.20261023T233607Z.000001.jsonl"
    source.write_bytes(FIXTURE.read_bytes())
    st = source.stat()
    entry = archive.Candidate(path=str(source), owner="example", sealing_contract="captured ledger copy",
                              device=st.st_dev, inode=st.st_ino, size=st.st_size,
                              mtime_ns=st.st_mtime_ns, sha256=hashlib.sha256(source.read_bytes()).hexdigest())
    output = tmp_path / "archive"
    output.mkdir(mode=0o700)
    limits = archive.Limits(max_input_bytes=1024 * 1024, max_output_bytes=2 * 1024 * 1024,
                            max_runtime_seconds=60, max_memory_bytes=1024 * 1024 * 1024,
                            min_free_bytes=1)
    config = archive.Config(version=1, service="cccc-history-zstd.service", source_root=str(root),
                            archive_root=str(output), limits=limits, candidates=[entry])
    return config, entry, source, output


def test_sidecar_roundtrip_and_recovery(candidate, monkeypatch, capsys):
    config, entry, source, output = candidate
    before = source.read_bytes()
    root = os.open(output, archive.DIR_FLAGS)
    try:
        name = archive.publish(config, entry, root, 60_000_000, config.limits.max_memory_bytes,
                               time.monotonic() + 60)
        assert name == archive.bundle_name(entry)
        archive.verify_existing(config, entry, root, name, time.monotonic() + 60)
        assert sorted(p.name for p in (output / name).iterdir()) == ["archive.zst", "receipt.json"]
        assert source.read_bytes() == before
    finally:
        os.close(root)
    monkeypatch.setattr(archive, "load_config", lambda _path: config)
    monkeypatch.setattr(archive, "systemd_contract", lambda _config: (60_000_000, config.limits.max_memory_bytes))
    monkeypatch.setattr(archive, "open_archive_root", lambda path: os.open(path, archive.DIR_FLAGS))
    monkeypatch.setattr(archive, "publish", lambda *_args: pytest.fail("existing bundle was republished"))
    monkeypatch.setattr(sys, "argv", ["history_zstd.py"])
    assert archive.main() == 0
    assert name + " already verified" in capsys.readouterr().out


def test_corrupt_final_is_held_and_original_retained(candidate):
    config, entry, source, output = candidate
    before = source.read_bytes()
    root = os.open(output, archive.DIR_FLAGS)
    try:
        name = archive.publish(config, entry, root, 60_000_000, config.limits.max_memory_bytes,
                               time.monotonic() + 60)
        path = output / name / "archive.zst"
        # Labeled mutation: damage one byte of a sidecar derived from real events.
        damaged = bytearray(path.read_bytes())
        damaged[-1] ^= 1
        path.write_bytes(damaged)
        with pytest.raises(ValueError, match="archive checksum differs"):
            archive.verify_existing(config, entry, root, name, time.monotonic() + 60)
        assert source.read_bytes() == before
        assert path.exists()
    finally:
        os.close(root)


def test_interrupted_staging_is_held_without_overwrite(candidate, monkeypatch):
    config, entry, source, output = candidate
    before = source.read_bytes()
    root = os.open(output, archive.DIR_FLAGS)
    try:
        def interruption(*_args, **_kwargs):
            raise TimeoutError("labeled interruption after reservation")
        monkeypatch.setattr(archive, "run_bounded", interruption)
        with pytest.raises(TimeoutError):
            archive.publish(config, entry, root, 60_000_000, config.limits.max_memory_bytes,
                            time.monotonic() + 60)
        assert len(list(output.glob(".staging-*"))) == 1
        assert source.read_bytes() == before
    finally:
        os.close(root)
    monkeypatch.setattr(archive, "load_config", lambda _path: config)
    monkeypatch.setattr(archive, "systemd_contract", lambda _config: (60_000_000, config.limits.max_memory_bytes))
    monkeypatch.setattr(archive, "open_archive_root", lambda path: os.open(path, archive.DIR_FLAGS))
    monkeypatch.setattr(sys, "argv", ["history_zstd.py"])
    with pytest.raises(ValueError, match="reconciliation"):
        archive.main()
    assert len(list(output.glob(".staging-*"))) == 1


def test_empty_manifest_opens_no_source_and_writes_no_bundle(candidate, monkeypatch):
    config, _entry, _source, output = candidate
    config.candidates = []
    monkeypatch.setattr(archive, "load_config", lambda _path: config)
    monkeypatch.setattr(archive, "systemd_contract", lambda _config: (60_000_000, config.limits.max_memory_bytes))
    monkeypatch.setattr(archive, "open_archive_root", lambda path: os.open(path, archive.DIR_FLAGS))
    monkeypatch.setattr(archive, "source_open", lambda *_args: pytest.fail("source was opened"))
    monkeypatch.setattr(sys, "argv", ["history_zstd.py"])
    assert archive.main() == 0
    assert list(output.iterdir()) == []


def test_changed_source_and_duplicate_json_are_refused(candidate):
    config, entry, source, _output = candidate
    directory, fd = archive.source_open(config, entry)
    try:
        # Labeled mutation of the observed source, not an invented event.
        with source.open("ab") as stream:
            stream.write(b"\n")
        with pytest.raises(ValueError, match="source identity differs"):
            archive.source_check(fd, entry)
    finally:
        os.close(fd)
        os.close(directory)
    # Labeled duplicate-key mutation of this source-bound manifest.
    manifest = config.model_dump_json().replace('"version":1', '"version":1,"version":1', 1)
    with pytest.raises(ValueError, match="duplicate JSON key"):
        archive.strict_json(manifest)


def test_publish_interruption_after_rename_is_recoverable(candidate, monkeypatch):
    config, entry, source, output = candidate
    before = source.read_bytes()
    root = os.open(output, archive.DIR_FLAGS)
    fsync = os.fsync
    try:
        def interrupted(fd):
            if (os.fstat(fd).st_dev, os.fstat(fd).st_ino) == (os.fstat(root).st_dev, os.fstat(root).st_ino):
                raise OSError("labeled interruption before archive-root fsync")
            return fsync(fd)
        with monkeypatch.context() as scoped:
            scoped.setattr(archive.os, "fsync", interrupted)
            with pytest.raises(OSError, match="labeled interruption"):
                archive.publish(config, entry, root, 60_000_000, config.limits.max_memory_bytes,
                                time.monotonic() + 60)
        name = archive.bundle_name(entry)
        assert (output / name).is_dir()
        archive.verify_existing(config, entry, root, name, time.monotonic() + 60)
        assert source.read_bytes() == before
    finally:
        os.close(root)


def test_reservation_failure_holds_staging_and_original(candidate, monkeypatch):
    config, entry, source, output = candidate
    before = source.read_bytes()
    root = os.open(output, archive.DIR_FLAGS)
    try:
        def no_space(*_args):
            raise OSError("labeled reservation failure")
        monkeypatch.setattr(archive.os, "posix_fallocate", no_space)
        with pytest.raises(OSError, match="reservation failure"):
            archive.publish(config, entry, root, 60_000_000, config.limits.max_memory_bytes,
                            time.monotonic() + 60)
        assert source.read_bytes() == before
        assert len(list(output.glob(".staging-*"))) == 1
        assert not (output / archive.bundle_name(entry)).exists()
    finally:
        os.close(root)


def test_replaced_source_symlink_is_refused(candidate):
    config, entry, source, _output = candidate
    original = source.with_suffix(".retained")
    source.rename(original)
    source.symlink_to(original.name)
    with pytest.raises(OSError):
        archive.source_open(config, entry)
    assert original.read_bytes() == FIXTURE.read_bytes()


def test_active_and_outside_paths_are_refused(candidate):
    config, entry, source, _output = candidate
    # Labeled path mutations of the captured candidate: neither a live ledger
    # name nor an outside-root name is admitted as a sealed segment.
    active = entry.model_copy(update={"path": str(source.parents[3] / "ledger.jsonl")})
    outside = entry.model_copy(update={"path": str(source.parents[5] / source.name)})
    with pytest.raises(ValueError, match="approved sealed"):
        archive.source_open(config, active)
    with pytest.raises(ValueError, match="outside"):
        archive.source_open(config, outside)
    assert source.read_bytes() == FIXTURE.read_bytes()


def test_incomplete_final_bundle_is_held(candidate):
    config, entry, source, output = candidate
    root = os.open(output, archive.DIR_FLAGS)
    try:
        name = archive.publish(config, entry, root, 60_000_000, config.limits.max_memory_bytes,
                               time.monotonic() + 60)
        receipt = output / name / "receipt.json"
        retained = output / "retained-receipt.json"
        # Labeled incomplete-publish mutation of the real-source bundle.
        receipt.rename(retained)
        with pytest.raises(ValueError, match="incomplete bundle"):
            archive.verify_existing(config, entry, root, name, time.monotonic() + 60)
        assert retained.is_file()
        assert (output / name / "archive.zst").is_file()
        assert source.read_bytes() == FIXTURE.read_bytes()
    finally:
        os.close(root)


@pytest.mark.parametrize("field", ["max_input_bytes", "max_output_bytes", "max_runtime_seconds",
                                   "max_memory_bytes", "min_free_bytes"])
def test_zero_resource_limit_is_refused(candidate, field):
    config, _entry, source, _output = candidate
    limits = config.limits.model_dump()
    limits[field] = 0  # Labeled mutation of the captured-source configuration.
    with pytest.raises(ValueError):
        archive.Limits.model_validate(limits)
    assert source.read_bytes() == FIXTURE.read_bytes()
