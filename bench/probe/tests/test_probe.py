# Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

"""CLI tests with temporary files and simulated guest processes, not a live boot."""

import contextlib
import copy
import hashlib
import io
import json
import os
import pathlib
import signal
import stat
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))
import probe
import guest
from test_guest import SimulatedRunner, IMAGE

STARTED = "2026-03-31T23:59:59Z"


def evidence(phase, grace=100):
    return {"schema": 1, "phase": phase, "edition": "ServerStandardEval",
            **dict.fromkeys(probe.ATTESTATIONS, True), "license_status": 2, "grace_minutes": grace}


class CliTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        self.images = self.root / "cache"
        self.results = self.root / "results"
        self.output = self.root / "output"
        self.environment = {"BENCH_STARTED_AT": STARTED, "RUNNER_TEMP": str(self.root),
                            "BENCH_MEMORY_MIB": "1024", "BENCH_DISK_GIB": "32",
                            "BENCH_TIMEOUT_SECONDS": "600", "BENCH_CLEANUP_SECONDS": "10"}
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.stack.enter_context(mock.patch.dict(os.environ, self.environment, clear=True))
        self.real_kvm = probe.require_kvm
        self.kvm = self.stack.enter_context(mock.patch.object(probe, "require_kvm"))
        self.preparing = self.stack.enter_context(mock.patch.object(guest, "prepare_image", side_effect=self.fake_prepare))
        self.verifying = self.stack.enter_context(mock.patch.object(guest, "verify_image", side_effect=self.fake_verify))
        self.private = []
        self.assertFalse(self.images.exists())
        self.assertFalse(self.results.exists())
        self.assertEqual(os.environ["BENCH_STARTED_AT"], STARTED)

    def private_file(self, work):
        self.private.append(work)
        self.assertEqual(stat.S_IMODE(work.stat().st_mode), 0o700)
        probe.disjoint(work, self.images)
        probe.disjoint(work, self.results)
        (work / "secret").write_text("SECRET")

    def fake_prepare(self, image, work):
        self.private_file(work)
        self.assertFalse(image.exists())
        self.assertFalse((self.images / "prepared.json").exists())
        image.write_bytes(IMAGE)
        source = guest.image_source()
        return {"compressed_bytes": len(IMAGE), "preparation_seconds": 7.0,
                "compression": probe.COMPRESSION, "guest_evidence": evidence("prepared"),
                "source_evidence": {"requested_url": source["url"],
                    "final_url": "https://download.microsoft.com/example.iso", "sha256": "a" * 64,
                    "sha256_kind": "measured", "publisher_sha256": None, "bytes": 9}}

    def fake_verify(self, image, work):
        self.private_file(work)
        self.assertEqual(image.read_bytes(), IMAGE)
        self.assertTrue((self.images / "prepared.json").is_file())
        return evidence("ready", 75)

    def invoke(self, command):
        argv = [command]
        if command == "identity":
            argv += ["--github-output", str(self.output)]
        else:
            argv += ["--result-dir", str(self.results)]
            if command != "kvm":
                argv += ["--image-dir", str(self.images)]
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            code = probe.main(argv)
        self.assertNotIn("SECRET", stdout.getvalue() + stderr.getvalue())
        return code, stdout.getvalue(), stderr.getvalue()

    def rows(self):
        content = (self.results / "probe.jsonl").read_text()
        self.assertNotIn("SECRET", content)
        return [json.loads(line) for line in content.splitlines()]

    def metadata(self):
        return json.loads((self.images / "prepared.json").read_text())

    def write_metadata(self, value):
        (self.images / "prepared.json").write_text(json.dumps(value))

    def assert_private_removed(self):
        self.assertTrue(self.private)
        self.assertTrue(all(not path.exists() for path in self.private))
        self.assertFalse(list(self.root.glob("bench-private-*")))
        self.assertFalse(list(self.images.glob(".prepare-*")))

    def test_kvm_checks_existence_write_access_and_open_before_dependencies(self):
        real_open = os.open
        for present, writable, opened in ((False, True, True), (True, False, True), (True, True, False), (True, True, True)):
            with self.subTest(present=present, writable=writable, opened=opened):
                def open_kvm(path, flags, *args, **kwargs):
                    if path == "/dev/kvm":
                        if not opened:
                            raise PermissionError("SECRET")
                        return real_open("/dev/null", flags)
                    return real_open(path, flags, *args, **kwargs)
                with mock.patch.object(probe, "require_kvm", side_effect=self.real_kvm), \
                     mock.patch.object(os.path, "exists", return_value=present), \
                     mock.patch.object(os, "access", return_value=writable) as access, \
                     mock.patch.object(os, "open", side_effect=open_kvm), \
                     mock.patch.object(probe, "inputs", side_effect=AssertionError("unexpected inputs")):
                    code, text, _ = self.invoke("kvm")
                expected = "kvm-available" if present and writable and opened else "no-kvm"
                self.assertEqual(text, "bench: " + expected + "\n")
                self.assertEqual(code, int(expected == "no-kvm"))
                self.assertEqual(self.rows()[-1]["status"], expected)
                if present:
                    access.assert_called_once_with("/dev/kvm", os.W_OK)
                else:
                    access.assert_not_called()
        self.preparing.assert_not_called()
        self.verifying.assert_not_called()

    def test_kvm_failure_stops_prepare_and_verify_before_identity(self):
        self.kvm.side_effect = guest.GuestError("kvm", "no-kvm")
        for command in ("prepare", "verify"):
            with mock.patch.object(probe, "inputs", side_effect=AssertionError("unexpected inputs")):
                self.assertEqual(self.invoke(command)[:2], (1, "bench: no-kvm\n"))
            self.assertEqual(self.rows()[-1]["code"], "no-kvm")
        self.assertFalse(self.images.exists())
        self.preparing.assert_not_called()
        self.verifying.assert_not_called()

    def test_identity_appends_only_the_exact_key(self):
        self.output.write_text("previous=value\n")
        expected = probe.current_identity()
        self.assertEqual(self.invoke("identity"), (0, "", ""))
        self.assertEqual(self.output.read_text(), "previous=value\ncache_key=" + expected["cache_key"] + "\n")
        self.assertFalse(self.results.exists())
        self.kvm.assert_not_called()

    def test_identity_failure_does_not_append(self):
        self.output.write_text("previous=value\n")
        for value in (None, "2026-13-01T00:00:00Z", "SECRET"):
            with self.subTest(value=value):
                if value is None:
                    os.environ.pop("BENCH_STARTED_AT", None)
                else:
                    os.environ["BENCH_STARTED_AT"] = value
                self.assertEqual(self.invoke("identity"), (1, "", "bench: identity-failed\n"))
                self.assertEqual(self.output.read_text(), "previous=value\n")

    def test_complete_input_set_and_runtime_settings_change_the_key(self):
        material = probe.inputs()
        self.assertEqual(set(material), {"bench/probe/probe.py", "bench/probe/cache.py",
            "bench/probe/runtime-settings.json", *guest.preparation_inputs()})
        self.assertFalse(any("/tests/" in path for path in material))
        original = probe.current_identity()
        for field in ("BENCH_MEMORY_MIB", "BENCH_DISK_GIB", "BENCH_TIMEOUT_SECONDS", "BENCH_CLEANUP_SECONDS"):
            with self.subTest(field=field), mock.patch.dict(os.environ, {field: str(int(os.environ[field]) + 1)}):
                self.assertNotEqual(probe.current_identity()["cache_key"], original["cache_key"])
        for field in ("BENCH_MEMORY_MIB", "BENCH_DISK_GIB", "BENCH_TIMEOUT_SECONDS", "BENCH_CLEANUP_SECONDS"):
            with self.subTest(missing=field):
                saved = os.environ.pop(field)
                try:
                    with self.assertRaises(guest.GuestError):
                        probe.current_identity()
                finally:
                    os.environ[field] = saved

    def test_input_bytes_paths_source_and_quarter_invalidate_identity(self):
        original = probe.current_identity()["cache_key"]
        material = probe.inputs()
        for changed in (dict(material, **{"bench/probe/probe.py": b"changed"}),
                        dict(material, **{"bench/probe/windows/new.ps1": b"new"}),
                        {key: value for key, value in material.items() if key != "bench/probe/cache.py"}):
            with mock.patch.object(probe, "inputs", return_value=changed):
                self.assertNotEqual(probe.current_identity()["cache_key"], original)
        source = guest.image_source()
        source["url"] += "&revision=2"
        with mock.patch.object(guest, "image_source", return_value=source):
            self.assertNotEqual(probe.current_identity()["cache_key"], original)
        with mock.patch.dict(os.environ, {"BENCH_STARTED_AT": "2026-04-01T00:00:00Z"}):
            self.assertNotEqual(probe.current_identity()["cache_key"], original)

    def test_preparation_inputs_reject_symlinks_and_missing_files(self):
        root = self.root / "repository"
        target = root / "bench/probe"
        target.mkdir(parents=True)
        (target / "probe.py").write_text("cli")
        (target / "cache.py").write_text("cache")
        with mock.patch.object(probe, "ROOT", root), mock.patch.object(guest, "preparation_inputs", return_value=["bench/probe/guest.py"]):
            with self.assertRaises(probe.ProbeError):
                probe.inputs()
            (target / "guest.py").symlink_to(target / "cache.py")
            with self.assertRaises(probe.ProbeError):
                probe.inputs()
            (target / "guest.py").unlink()
            (target / "guest.py").write_text("guest")
            self.assertEqual(probe.inputs()["bench/probe/guest.py"], b"guest")

    def test_prepare_and_verify_publish_the_pair_and_final_measurements(self):
        self.assertEqual(self.invoke("prepare")[:2], (0, "bench: prepared\n"))
        self.assertEqual(set(path.name for path in self.images.iterdir()), {"image.qcow2", "prepared.json"})
        stored = self.metadata()
        self.assertEqual(set(stored), {"version", "provenance", "preparation"})
        self.assertEqual(stored["provenance"]["image"]["sha256"], hashlib.sha256(IMAGE).hexdigest())
        self.assertEqual(stat.S_IMODE((self.images / "prepared.json").stat().st_mode), 0o600)
        self.assertEqual(self.invoke("verify")[:2], (0, "bench: ok\n"))
        row = self.rows()[-1]
        self.assertEqual(row["event"], "probe-verify")
        self.assertEqual(row["status"], "ok")
        self.assertEqual(row["compressed_bytes"], len(IMAGE))
        self.assertEqual(row["preparation_seconds"], 7.0)
        self.assertEqual(row["guest_evidence"], evidence("ready", 75))
        self.assertEqual(row["preparation"]["guest_evidence"], evidence("prepared"))
        self.assertEqual(row["preparation"]["source_evidence"]["sha256_kind"], "measured")
        self.assertIsNone(row["installation_time"])
        self.assertEqual(row["installation_time_status"], "unavailable")
        self.assertEqual(self.metadata(), stored)
        self.assert_private_removed()

    def test_a_quarter_crossing_keeps_the_workflow_start(self):
        key = probe.current_identity()["cache_key"]
        with mock.patch("time.time", return_value=1775001601):
            self.assertEqual(self.invoke("prepare")[0], 0)
            self.assertEqual(self.invoke("verify")[0], 0)
            self.assertEqual(self.invoke("identity")[0], 0)
        self.assertEqual(self.output.read_text(), "cache_key=" + key + "\n")
        self.assertEqual(self.rows()[-1]["cache_key"], key)
        self.assertEqual(self.metadata()["preparation"]["started_at"], STARTED)
        with mock.patch.dict(os.environ, {"BENCH_STARTED_AT": "2026-04-01T00:00:01Z"}):
            self.verifying.reset_mock()
            self.assertEqual(self.invoke("verify")[0], 1)
            self.verifying.assert_not_called()

    def test_cache_hit_preserves_preparation_evidence_for_another_job(self):
        self.assertEqual(self.invoke("prepare")[0], 0)
        original = self.metadata()
        with mock.patch.dict(os.environ, {"BENCH_STARTED_AT": "2026-03-31T23:59:59.900000Z"}):
            self.assertEqual(self.invoke("verify")[0], 0)
        row = self.rows()[-1]
        self.assertNotEqual(row["started_at"], row["preparation"]["started_at"])
        self.assertEqual(row["preparation"], original["preparation"])
        self.assertEqual(self.metadata(), original)

    def test_invalid_cache_never_reaches_guest(self):
        self.assertEqual(self.invoke("prepare")[0], 0)
        original = self.metadata()
        variants = [None, [], {}, {**original, "version": True}, {**original, "version": 2}, {**original, "secret": "SECRET"}]
        for field, value in (("quarter", "2025-Q1"), ("preparation_digest", "b" * 64), ("source", {"changed": True})):
            changed = copy.deepcopy(original)
            changed["provenance"]["identity"][field] = value
            variants.append(changed)
        changed = copy.deepcopy(original)
        changed["provenance"]["measurements"]["compressed_bytes"] += 1
        variants.append(changed)
        for value in variants:
            with self.subTest(value=value):
                self.write_metadata(value)
                self.assertEqual(self.invoke("verify")[0], 1)
                self.assertEqual(self.rows()[-1]["status"], "verify-failed")
        for raw in ("{SECRET", '{"version":1,"version":1}', '{"version":NaN}'):
            (self.images / "prepared.json").write_text(raw)
            self.assertEqual(self.invoke("verify")[0], 1)
        self.verifying.assert_not_called()
        self.assertEqual((self.images / "image.qcow2").read_bytes(), IMAGE)

    def test_metadata_symlink_and_image_tampering_are_rejected(self):
        self.assertEqual(self.invoke("prepare")[0], 0)
        metadata = self.images / "prepared.json"
        saved = metadata.read_bytes()
        outside = self.root / "elsewhere"
        outside.write_bytes(saved)
        metadata.unlink()
        metadata.symlink_to(outside)
        self.assertEqual(self.invoke("verify")[0], 1)
        self.assertEqual(outside.read_bytes(), saved)
        metadata.unlink()
        metadata.write_bytes(saved)
        (self.images / "image.qcow2").write_bytes(b"different")
        self.assertEqual(self.invoke("verify")[0], 1)
        self.verifying.assert_not_called()

    def test_preparation_failure_never_publishes_metadata_or_success(self):
        def fail(image, work):
            self.fake_prepare(image, work)
            raise guest.GuestError("prepare", "command-failed", ("dependency-failed",))
        self.preparing.side_effect = fail
        self.assertEqual(self.invoke("prepare")[:2], (1, "bench: prepare-failed\n"))
        self.assertEqual(self.rows()[-1]["stage"], "prepare")
        self.assertEqual(self.rows()[-1]["cleanup"], ["dependency-failed"])
        self.assertEqual(list(self.images.iterdir()), [])
        self.assert_private_removed()

    def test_ssh_failure_and_wrong_ready_evidence_never_print_ok(self):
        self.assertEqual(self.invoke("prepare")[0], 0)
        original = self.metadata()
        def fail(image, work):
            self.fake_verify(image, work)
            raise guest.GuestError("ssh", "command-failed")
        self.verifying.side_effect = fail
        self.assertEqual(self.invoke("verify")[:2], (1, "bench: verify-failed\n"))
        self.assertEqual(self.rows()[-1]["stage"], "ssh")
        for value in (None, {**evidence("ready"), "nonce": "SECRET"}, evidence("prepared"),
                      {**evidence("ready"), "license_status": 2, "grace_minutes": 0},
                      {**evidence("ready"), "edition": "ServerDatacenterEval"}):
            self.verifying.side_effect = lambda image, work, value=value: value
            self.assertEqual(self.invoke("verify")[:2], (1, "bench: verify-failed\n"))
        self.assertEqual(self.metadata(), original)
        self.assert_private_removed()

    def test_bad_preparation_evidence_and_measurements_are_rejected(self):
        for field, value in (("compressed_bytes", len(IMAGE) + 1), ("preparation_seconds", float("nan")),
                             ("compression", "SECRET"), ("guest_evidence", {**evidence("prepared"), "extra": "SECRET"}),
                             ("source_evidence", {"secret": "SECRET"})):
            with self.subTest(field=field):
                def bad(image, work):
                    measured = self.fake_prepare(image, work)
                    measured[field] = value
                    return measured
                self.preparing.side_effect = bad
                self.assertEqual(self.invoke("prepare")[0], 1)
                self.assertEqual(list(self.images.iterdir()), [])
                self.assert_private_removed()

    def test_untrusted_cached_evidence_is_not_uploaded(self):
        self.assertEqual(self.invoke("prepare")[0], 0)
        original = self.metadata()
        for field, value in (("final_url", "https://download.microsoft.com/x?secret=SECRET"),
                             ("sha256", "SECRET"), ("bytes", True), ("publisher_sha256", "b" * 64)):
            changed = copy.deepcopy(original)
            changed["preparation"]["source_evidence"][field] = value
            self.write_metadata(changed)
            self.assertEqual(self.invoke("verify")[0], 1)
        self.verifying.assert_not_called()

    def test_publication_failure_removes_the_new_image(self):
        real_link = os.link
        def failed_link(source, target, **kwargs):
            if pathlib.Path(target).name == "prepared.json":
                raise OSError("SECRET")
            return real_link(source, target, **kwargs)
        with mock.patch.object(os, "link", side_effect=failed_link):
            self.assertEqual(self.invoke("prepare")[0], 1)
        self.assertEqual(list(self.images.iterdir()), [])
        self.assert_private_removed()

    def test_existing_cache_is_not_replaced(self):
        self.assertEqual(self.invoke("prepare")[0], 0)
        original = self.metadata()
        self.preparing.reset_mock()
        self.assertEqual(self.invoke("prepare")[0], 1)
        self.assertEqual(self.metadata(), original)
        self.assertEqual((self.images / "image.qcow2").read_bytes(), IMAGE)
        self.preparing.assert_not_called()

    def test_overlapping_cache_and_results_stop_before_preparation(self):
        self.images = self.results / "cache"
        self.assertEqual(self.invoke("prepare")[0], 1)
        self.preparing.assert_not_called()
        self.assertFalse(self.images.exists())

    def test_cleanup_error_is_not_success(self):
        real_cleanup = tempfile.TemporaryDirectory.cleanup
        def failed_cleanup(temporary):
            real_cleanup(temporary)
            raise OSError("SECRET")
        with mock.patch.object(tempfile.TemporaryDirectory, "cleanup", failed_cleanup):
            self.assertEqual(self.invoke("prepare")[:2], (1, "bench: prepare-failed\n"))
        self.assertEqual(self.rows()[-1]["stage"], "cleanup")
        self.assert_private_removed()

    def test_interruptions_clean_private_files_and_restore_handlers(self):
        self.assertEqual(self.invoke("prepare")[0], 0)
        for kind in ("keyboard", signal.SIGINT, signal.SIGTERM):
            before = {signum: signal.getsignal(signum) for signum in (signal.SIGINT, signal.SIGTERM)}
            def interrupted(image, work):
                result = self.fake_verify(image, work)
                if kind == "keyboard":
                    raise KeyboardInterrupt()
                signal.raise_signal(kind)
                return result
            self.verifying.side_effect = interrupted
            self.assertEqual(self.invoke("verify")[:2], (1, "bench: verify-failed\n"))
            self.assertEqual(self.rows()[-1]["stage"], "interrupted")
            self.assertEqual({signum: signal.getsignal(signum) for signum in before}, before)
            self.assert_private_removed()

    def test_result_write_failure_cannot_report_ok(self):
        self.assertEqual(self.invoke("prepare")[0], 0)
        with mock.patch.object(probe, "append_result", side_effect=OSError("SECRET")):
            self.assertEqual(self.invoke("verify"), (1, "", "bench: result-failed\n"))
        self.assertEqual(len(self.rows()), 1)
        self.assert_private_removed()

    def test_result_symlink_is_not_followed(self):
        self.results.mkdir()
        outside = self.root / "outside"
        outside.write_text("unchanged")
        (self.results / "probe.jsonl").symlink_to(outside)
        self.assertEqual(self.invoke("kvm"), (1, "", "bench: result-failed\n"))
        self.assertEqual(outside.read_text(), "unchanged")

    def test_cli_uses_both_real_modules_with_simulated_processes(self):
        prepare_runner = SimulatedRunner(["prepared"])
        verify_runner = SimulatedRunner(["ready", "stopping"])
        self.preparing.side_effect = lambda image, work: REAL_PREPARE(image, work, runner=prepare_runner)
        self.verifying.side_effect = lambda image, work: REAL_VERIFY(image, work, runner=verify_runner)
        self.assertEqual(self.invoke("prepare")[0], 0)
        self.assertEqual(self.invoke("verify")[:2], (0, "bench: ok\n"))
        self.assertEqual(self.rows()[-1]["compressed_bytes"], len(IMAGE))
        self.assertEqual(self.rows()[-1]["guest_evidence"], evidence("ready"))
        for runner in (prepare_runner, verify_runner):
            self.assertEqual(runner.phases, [])
            self.assertTrue(all(process.ended for process in runner.processes))
            self.assertTrue(all(channel.closed for channel in runner.channels))
            self.assertEqual([process for process, _ in runner.reaped], runner.processes)
        self.assertFalse(list(self.root.glob("bench-private-*")))
        self.assertEqual(set(path.name for path in self.images.iterdir()), {"image.qcow2", "prepared.json"})


REAL_PREPARE = guest.prepare_image
REAL_VERIFY = guest.verify_image

if __name__ == "__main__":
    unittest.main()
