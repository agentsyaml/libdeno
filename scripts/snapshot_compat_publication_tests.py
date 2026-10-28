"""Focused publication-lock tests imported by test-snapshot-compat.py."""

import shutil
import threading
from pathlib import Path
from unittest import mock


def generated_stage(root, name):
    stage = root / name
    stage.mkdir()
    (stage / "libdeno-snapshot.manifest").write_bytes(
        f"format=libdeno-snapshot-2\nproducer={name}\n".encode("utf-8"))
    (stage / "CLI_SNAPSHOT.bin").write_text(name)
    (stage / "EXTENSION_RESIDUAL_SOURCES.rs").write_text("table")
    return stage


def test_concurrent_publishers(compat, root, target):
    destination = root / "racing-bundle"
    first, second = generated_stage(root, "first-stage"), generated_stage(root, "second-stage")
    entered, release = threading.Event(), threading.Event()
    failures, real_copy = [], shutil.copy2

    def blocked_copy(source, output):
        try:
            Path(source).relative_to(first)
            from_first = True
        except ValueError:
            from_first = False
        if from_first and not entered.is_set():
            entered.set()
            assert release.wait(10), "publisher barrier timed out"
        return real_copy(source, output)

    def publish_first():
        try:
            compat.publish_bundle(first, destination, target, "a" * 64)
        except BaseException as error:
            failures.append(error)

    with mock.patch.object(compat.shutil, "copy2", side_effect=blocked_copy):
        thread = threading.Thread(target=publish_first)
        thread.start()
        assert entered.wait(10), "first publisher did not enter its copy section"
        try:
            compat.publish_bundle(second, destination, target, "b" * 64)
        except ValueError as error:
            assert "already owns lock" in str(error)
        else:
            raise AssertionError("competing publisher was not rejected")
        release.set()
        thread.join(10)
    assert not thread.is_alive() and not failures, str(failures)
    record = compat.read_record(destination / compat.SIDECAR)
    assert record["source-hash"] == "a" * 64
    assert record["manifest-hash"] == compat.fnv((first / "libdeno-snapshot.manifest").read_bytes())
    assert (destination / "CLI_SNAPSHOT.bin").read_text() == "first-stage"
    assert not compat.publication_lock_path(destination).exists()
    stale_lock = compat.publication_lock_path(destination)
    stale_lock.mkdir()
    try:
        compat.publish_bundle(second, destination, target, "b" * 64)
    except ValueError as error:
        assert "remove the stale lock directory manually" in str(error)
    else:
        raise AssertionError("stale publication lock was silently stolen")
    stale_lock.rmdir()


def test_case_alias_lock(compat, root):
    destination = root / "CaseAliasBundle"
    alias = root / "casealiasbundle"
    destination.mkdir()
    if not alias.exists():
        alias.mkdir()
    if not alias.samefile(destination):
        if alias != destination:
            alias.rmdir()
        print("case-alias lock test skipped (case-sensitive filesystem)")
        return
    with compat.publication_lock(destination):
        try:
            with compat.publication_lock(alias):
                pass
        except ValueError as error:
            assert "already owns lock" in str(error)
        else:
            raise AssertionError("case-alias destination bypassed publication lock")
    assert not compat.publication_lock_path(destination).exists()
    print("case-alias publication lock test passed")
