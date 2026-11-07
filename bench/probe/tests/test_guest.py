# Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

"""Deterministic host tests: python3 -m unittest discover -s bench/probe/tests.

The process and guest events below are simulated. Script assertions inspect
configuration only; they are not evidence of a Windows installation or boot.
"""

import hashlib
import json
import os
import pathlib
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import xml.etree.ElementTree as ET

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))
import guest  # noqa: E402

ISO = b"simulated Microsoft ISO"
IMAGE = b"simulated compressed qcow2"
PUBLIC = b"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample bench\n"
WIM = b'''<WIM>
<IMAGE INDEX="1"><DISPLAYNAME>Windows Server 2025 Standard Evaluation</DISPLAYNAME><WINDOWS><INSTALLATIONTYPE>Server Core</INSTALLATIONTYPE><EDITIONID>ServerStandardEval</EDITIONID><ARCH>9</ARCH></WINDOWS></IMAGE>
<IMAGE INDEX="2"><DISPLAYNAME>Windows Server 2025 Standard Evaluation (Desktop Experience)</DISPLAYNAME><WINDOWS><INSTALLATIONTYPE>Server</INSTALLATIONTYPE><EDITIONID>ServerStandardEval</EDITIONID><ARCH>9</ARCH></WINDOWS></IMAGE>
</WIM>'''


class Process:
    pid = 123456

    def __init__(self, runner):
        self.runner = runner
        self.ended = False
        self.waits = []

    def wait(self, timeout):
        self.waits.append(timeout)
        self.runner.inject("wait")
        self.ended = True
        return self.runner.exit_code


class Events:
    def __init__(self, runner, directory):
        self.runner = runner
        self.path = directory / "serial.sock"
        self.closed = False

    def __enter__(self):
        return self

    def attach(self, process):
        self.runner.inject("attach")

    def receive(self, deadline):
        self.runner.inject("event")
        phase = self.runner.phases.pop(0)
        record = {"schema": 1, "nonce": self.runner.seeds[-1]["metadata"]["nonce"], "phase": phase,
                  "edition": "ServerStandardEval", "setup_complete": True,
                  "password_invalidated": True, "base_accounts_disabled": True,
                  "base_host_keys_absent": True, "license_status": 2, "grace_minutes": 100}
        record.update(self.runner.event_changes)
        return record

    def __exit__(self, *_):
        self.closed = True


class SimulatedRunner:
    def __init__(self, phases):
        self.now = 1000
        self.phases = list(phases)
        self.commands = []
        self.seeds = []
        self.processes = []
        self.channels = []
        self.reaped = []
        self.failure = None
        self.event_changes = {}
        self.exit_code = 0
        self.available_memory = 1 << 40
        self.available_disk = 1 << 40
        self.final_url = "https://download.microsoft.com/example.iso"
        self.kvm_calls = 0
        self.ssh_reply = b"BENCH_SSH_OK\r\n"
        self.shutdown_disconnect = False
        self.ssl_verify_result = 0
        self.size_download = len(ISO)
        self.disk_after_download = None
        self.private_modes = {}
        self.signal_after_popen = None
        self.reap_timeout = False

    def clock(self):
        return self.now

    def inject(self, operation):
        if self.failure == operation:
            raise subprocess.CalledProcessError(1, [operation], output=b"SECRET", stderr=b"SECRET")
        if self.failure == "timeout-" + operation:
            raise subprocess.TimeoutExpired([operation], 1, output=b"SECRET")
        if self.failure == "interrupt-" + operation:
            raise KeyboardInterrupt()
        if self.failure == "term-" + operation:
            signal.raise_signal(signal.SIGTERM)

    def kvm(self):
        self.kvm_calls += 1
        if self.failure == "kvm":
            raise guest.GuestError("kvm", "no-kvm")
        self.inject("kvm")

    def memory_bytes(self):
        return self.available_memory

    def disk_bytes(self, path):
        return self.available_disk

    def run(self, command, timeout):
        self.commands.append((command, timeout))
        self.now += 1
        self.inject(command[0])
        if command[0] == "curl":
            pathlib.Path(command[command.index("--output") + 1]).write_bytes(ISO)
            if self.disk_after_download is not None:
                self.available_disk = self.disk_after_download
            return json.dumps({"url_effective": self.final_url, "ssl_verify_result": self.ssl_verify_result,
                               "size_download": self.size_download}).encode()
        if command[0] == "7z":
            pathlib.Path(command[3][2:], "install.wim").write_bytes(b"WIM")
        elif command[0] == "wimlib-imagex":
            return WIM
        elif command[0] == "genisoimage":
            tree = pathlib.Path(command[-1])
            files = {str(p.relative_to(tree)): p.read_bytes() for p in tree.rglob("*") if p.is_file()}
            self.seeds.append({"metadata": json.loads(files["seed.json"]), "files": files,
                               "mode": stat.S_IMODE(tree.stat().st_mode), "path": tree})
            pathlib.Path(command[command.index("-o") + 1]).write_bytes(b"SEED")
        elif command[:2] == ["qemu-img", "create"]:
            path = command[-1] if "-b" in command else command[-2]
            pathlib.Path(path).write_bytes(b"IMAGE")
        elif command[:2] == ["qemu-img", "convert"]:
            self.inject("convert")
            pathlib.Path(command[-1]).write_bytes(IMAGE)
        elif command[0] == "ssh-keygen":
            path = pathlib.Path(command[command.index("-f") + 1])
            if "-y" in command:
                return PUBLIC
            path.write_bytes(b"PRIVATE-" + path.name.encode())
            path.with_suffix(".pub").write_bytes(PUBLIC)
            path.with_suffix(".pub").chmod(0o644)
        elif command[0] == "ssh":
            self.inject("shutdown" if command[-1].endswith("shutdown.ps1") else "verify-command")
            key = pathlib.Path(command[command.index("-i") + 1])
            if stat.S_IMODE(key.stat().st_mode) != 0o600:
                raise AssertionError("private key mode")
            self.hosts = pathlib.Path(next(v.split("=", 1)[1] for v in command if v.startswith("UserKnownHostsFile="))).read_text()
            if command[-1].endswith("shutdown.ps1") and self.shutdown_disconnect:
                raise subprocess.CalledProcessError(255, command)
            return self.ssh_reply
        return b""

    def popen(self, command):
        self.now += 1
        self.commands.append((command, None))
        self.inject("popen")
        directory = self.channels[-1].path.parent
        self.private_modes = {str(p.relative_to(directory)): stat.S_IMODE(p.stat().st_mode)
                              for p in directory.rglob("*")}
        process = Process(self)
        self.processes.append(process)
        if self.signal_after_popen:
            signal.raise_signal(self.signal_after_popen)
        return process

    def channel(self, directory):
        channel = Events(self, directory)
        self.channels.append(channel)
        return channel

    def reap(self, process, timeout):
        self.reaped.append((process, timeout))
        process.ended = True
        if self.reap_timeout:
            raise subprocess.TimeoutExpired(["qemu"], timeout, output=b"SECRET")
        self.inject("reap")


class BackendTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        self.image = self.root / "cache" / "image.qcow2"
        self.work = self.root / "private"
        self.settings = guest.Settings(1024, 32, 600, 10)

    def prepare(self, runner):
        return guest.prepare_image(self.image, self.work, settings=self.settings, runner=runner)

    def verify(self, runner):
        self.image.parent.mkdir(exist_ok=True)
        self.image.write_bytes(IMAGE)
        return guest.verify_image(self.image, self.work, settings=self.settings, runner=runner)

    def assert_clean(self, runner):
        if self.work.exists():
            self.assertEqual(list(self.work.iterdir()), [])
        self.assertEqual([p for p, _ in runner.reaped], runner.processes)
        self.assertTrue(all(p.ended for p in runner.processes))
        self.assertTrue(all(c.closed for c in runner.channels))
        self.assertTrue(all(timeout == 10 for _, timeout in runner.reaped))

    def test_prepare_measures_only_after_attestation_shutdown_and_compression(self):
        runner = SimulatedRunner(["prepared"])
        result = self.prepare(runner)
        self.assertEqual(self.image.read_bytes(), IMAGE)
        self.assertEqual(result["compressed_bytes"], len(IMAGE))
        self.assertEqual(result["preparation_seconds"], 7)
        self.assertIsInstance(result["preparation_seconds"], float)
        self.assertIn("zlib", result["compression"])
        self.assertEqual(result["source_evidence"]["sha256"], hashlib.sha256(ISO).hexdigest())
        self.assertEqual(result["source_evidence"]["sha256_kind"], "measured")
        self.assertIsNone(result["source_evidence"]["publisher_sha256"])
        self.assertEqual(result["source_evidence"]["final_url"], runner.final_url)
        self.assertEqual(result["guest_evidence"]["edition"], "ServerStandardEval")
        self.assertEqual(list(self.image.parent.iterdir()), [self.image])
        self.assertEqual(stat.S_IMODE(self.image.stat().st_mode), 0o600)
        seed = runner.seeds[0]
        self.assertEqual(seed["mode"], 0o700)
        self.assertNotIn("host_key", seed["files"])
        self.assertNotIn("client.pub", seed["files"])
        self.assertNotIn(b"__ADMIN_PASSWORD__", seed["files"]["autounattend.xml"])
        self.assertNotIn(b"__IMAGE_INDEX__", seed["files"]["autounattend.xml"])
        qemu = next(c for c, _ in runner.commands if c[0] == "qemu-system-x86_64")
        self.assertEqual(qemu[qemu.index("-accel") + 1], "kvm")
        self.assertEqual(qemu[qemu.index("-nic") + 1], "none")
        self.assertNotIn("tcg", qemu)
        convert = next(c for c, _ in runner.commands if c[:2] == ["qemu-img", "convert"])
        self.assertIn("-c", convert)
        self.assertIn("compression_type=zlib", convert)
        self.assertEqual(runner.kvm_calls, 1)
        curl, timeout = next((c, t) for c, t in runner.commands if c[0] == "curl")
        self.assertEqual(curl[curl.index("--proto") + 1], "=https")
        self.assertEqual(curl[curl.index("--proto-redir") + 1], "=https")
        self.assertEqual(float(curl[curl.index("--max-time") + 1]), timeout)
        self.assertEqual(timeout, self.settings.timeout_seconds)
        self.assert_clean(runner)

    def test_prepare_failure_and_interruptions_do_not_publish(self):
        for failure in ("kvm", "curl", "7z", "wimlib-imagex", "genisoimage", "qemu-img", "popen", "attach", "event", "wait", "convert", "timeout-event", "interrupt-event", "term-event"):
            with self.subTest(failure=failure):
                runner = SimulatedRunner(["prepared"])
                runner.failure = failure
                with self.assertRaises((guest.GuestError, KeyboardInterrupt)) as error:
                    self.prepare(runner)
                self.assertNotIn("SECRET", str(error.exception))
                if failure == "kvm":
                    self.assertEqual(runner.commands, [])
                    self.assertEqual(error.exception.code, "no-kvm")
                self.assertFalse(self.image.exists())
                self.assert_clean(runner)

    def test_prepare_rejects_guest_claims_and_unclean_exit(self):
        cases = ({"phase": "failed"}, {"nonce": "different"}, {"edition": "ServerDatacenterEval"},
                 {"password_invalidated": False}, {"setup_complete": False},
                 {"base_host_keys_absent": False}, {"base_accounts_disabled": False},
                 {"license_status": 0}, {"license_status": 2, "grace_minutes": 0})
        for changes in cases:
            with self.subTest(changes=changes):
                runner = SimulatedRunner(["prepared"])
                runner.event_changes = changes
                with self.assertRaises(guest.GuestError):
                    self.prepare(runner)
                self.assertFalse(self.image.exists())
                self.assert_clean(runner)
        runner = SimulatedRunner(["prepared"])
        runner.exit_code = 1
        with self.assertRaisesRegex(guest.GuestError, "qemu-failed"):
            self.prepare(runner)
        self.assertFalse(self.image.exists())
        self.assert_clean(runner)

    def test_verify_enforces_host_identity_loopback_and_overlay(self):
        runner = SimulatedRunner(["ready", "stopping"])
        self.assertIsNone(self.verify(runner))
        self.assertEqual(self.image.read_bytes(), IMAGE)
        qemu = next(c for c, _ in runner.commands if c[0] == "qemu-system-x86_64")
        network = qemu[qemu.index("-netdev") + 1]
        self.assertRegex(network, r"\Auser,id=net,restrict=on,hostfwd=tcp:127\.0\.0\.1:[0-9]+-:22\Z")
        self.assertEqual(qemu[qemu.index("-accel") + 1], "kvm")
        self.assertTrue(any("overlay.qcow2" in item for item in qemu))
        create = next(c for c, _ in runner.commands if c[:2] == ["qemu-img", "create"])
        self.assertEqual(create[create.index("-b") + 1], str(self.image.resolve()))
        self.assertEqual(create[create.index("-F") + 1], "qcow2")
        ssh = [c for c, _ in runner.commands if c[0] == "ssh"]
        self.assertEqual(len(ssh), 2)
        for command in ssh:
            for option in ("StrictHostKeyChecking=yes", "GlobalKnownHostsFile=/dev/null", "IdentityAgent=none",
                           "IdentitiesOnly=yes", "HostKeyAlgorithms=ssh-ed25519", "PasswordAuthentication=no",
                           "KbdInteractiveAuthentication=no", "PreferredAuthentications=publickey", "ClearAllForwardings=yes"):
                self.assertIn(option, command)
            self.assertEqual(command[command.index("-F") + 1], "/dev/null")
            self.assertIn("benchprobe@127.0.0.1", command)
        nonce = runner.seeds[0]["metadata"]["nonce"]
        self.assertEqual(runner.hosts, "bench-" + nonce + " " + b" ".join(PUBLIC.split()[:2]).decode() + "\n")
        seed = runner.seeds[0]
        self.assertEqual(set(seed["files"]), {"seed.json", "benchrun.tag", "host_key", "client.pub"})
        self.assertEqual(seed["files"]["host_key"], b"PRIVATE-host")
        self.assertEqual(seed["mode"], 0o700)
        self.assertEqual(runner.private_modes["seed"], 0o700)
        for path in ("client", "client.pub", "host", "host.pub", "known_hosts", "seed.iso",
                     "seed/host_key", "seed/client.pub", "seed/seed.json"):
            self.assertEqual(runner.private_modes[path], 0o600, path)
        self.assertEqual(runner.kvm_calls, 1)
        self.assertNotIn(b"PRIVATE-client", b"".join(seed["files"].values()))
        self.assert_clean(runner)

    def test_verify_failures_always_reap_and_remove_secrets_without_changing_base(self):
        for failure in ("kvm", "ssh-keygen", "genisoimage", "qemu-img", "popen", "attach", "event", "ssh", "verify-command", "shutdown", "wait", "interrupt-event", "term-verify-command"):
            with self.subTest(failure=failure):
                runner = SimulatedRunner(["ready", "stopping"])
                runner.failure = failure
                with self.assertRaises((guest.GuestError, KeyboardInterrupt)) as error:
                    self.verify(runner)
                self.assertNotIn("SECRET", str(error.exception))
                if failure == "kvm":
                    self.assertEqual(runner.commands, [])
                    self.assertEqual(error.exception.code, "no-kvm")
                self.assertEqual(runner.kvm_calls, 1)
                self.assertEqual(self.image.read_bytes(), IMAGE)
                self.assert_clean(runner)

    def test_wrong_ssh_output_and_shutdown_disconnect(self):
        runner = SimulatedRunner(["ready", "stopping"])
        runner.ssh_reply = b"SECRET"
        with self.assertRaisesRegex(guest.GuestError, "wrong-command-result") as error:
            self.verify(runner)
        self.assertNotIn("SECRET", str(error.exception))
        self.assert_clean(runner)
        runner = SimulatedRunner(["ready", "stopping"])
        runner.shutdown_disconnect = True
        self.assertIsNone(self.verify(runner))
        self.assertEqual(runner.phases, [])
        self.assert_clean(runner)
        runner = SimulatedRunner(["ready", "failed"])
        runner.shutdown_disconnect = True
        with self.assertRaisesRegex(guest.GuestError, "unexpected-phase"):
            self.verify(runner)
        self.assert_clean(runner)

    def test_serial_evidence_exports_only_declared_fields(self):
        runner = SimulatedRunner(["prepared"])
        runner.event_changes = {"extra_secret": "SECRET"}
        evidence = self.prepare(runner)["guest_evidence"]
        self.assertEqual(set(evidence), {"schema", "phase", "setup_complete", "password_invalidated",
                                      "base_accounts_disabled", "base_host_keys_absent", "edition",
                                      "license_status", "grace_minutes"})
        self.assertNotIn("SECRET", json.dumps(evidence))
        self.assertNotIn("nonce", evidence)
        self.assert_clean(runner)

    def test_certificate_and_download_length_fail_closed(self):
        for field, value, code in (("ssl_verify_result", 1, "untrusted-download"),
                                   ("size_download", len(ISO) + 1, "download-size")):
            with self.subTest(field=field):
                runner = SimulatedRunner(["prepared"])
                setattr(runner, field, value)
                with self.assertRaisesRegex(guest.GuestError, code):
                    self.prepare(runner)
                self.assertFalse(self.image.exists())
                self.assert_clean(runner)

    def test_capacity_is_checked_after_the_download(self):
        runner = SimulatedRunner(["prepared"])
        runner.disk_after_download = 2 * self.settings.disk_gib * 1024 ** 3 - 1
        with self.assertRaisesRegex(guest.GuestError, "no-room"):
            self.prepare(runner)
        self.assertEqual(runner.processes, [])
        self.assertFalse(self.image.exists())
        self.assert_clean(runner)

    def test_cleanup_failure_is_an_error_in_both_phases(self):
        for operation, phases in ((self.prepare, ["prepared"]), (self.verify, ["ready", "stopping"])):
            runner = SimulatedRunner(phases)
            runner.failure = "reap"
            with self.assertRaises(guest.GuestError) as error:
                operation(runner)
            self.assertEqual(error.exception.stage, "cleanup")
            self.assertNotIn("SECRET", str(error.exception))
            self.assert_clean(runner)

    def test_verification_requires_a_clean_qemu_exit(self):
        runner = SimulatedRunner(["ready", "stopping"])
        runner.exit_code = 1
        with self.assertRaisesRegex(guest.GuestError, "qemu-failed"):
            self.verify(runner)
        self.assertEqual(self.image.read_bytes(), IMAGE)
        self.assert_clean(runner)

    def test_publisher_hash_must_match_when_supplied(self):
        source = guest.image_source()
        source["publisher_sha256"] = "0" * 64
        runner = SimulatedRunner(["prepared"])
        with mock.patch("guest.image_source", return_value=source):
            with self.assertRaisesRegex(guest.GuestError, "publisher-digest-mismatch"):
                self.prepare(runner)
        self.assertFalse(self.image.exists())
        self.assert_clean(runner)

    def test_refuses_bad_source_or_ambiguous_edition(self):
        for url in ("http://download.microsoft.com/x.iso", "https://example.invalid/x.iso", "https://download.microsoft.com/x?token=secret"):
            runner = SimulatedRunner(["prepared"])
            runner.final_url = url
            with self.assertRaisesRegex(guest.GuestError, "untrusted-download"):
                self.prepare(runner)
            self.assertFalse(self.image.exists())
            self.assert_clean(runner)
        source = guest.image_source()
        self.assertEqual(guest.image_record(WIM, source), {"index": 1, "edition": "ServerStandardEval"})
        for xml in (WIM.replace(b"<ARCH>9", b"<ARCH>12"), WIM.replace(b"ServerStandardEval", b"ServerStandard"),
                    WIM.replace(b"</WIM>", WIM[5:])):
            with self.assertRaises((guest.GuestError, ET.ParseError)):
                guest.image_record(xml, source)

    def test_refuses_unavailable_capacity_at_both_sides_of_each_boundary(self):
        memory = self.settings.memory_mib * 1024 * 1024
        disk = 2 * self.settings.disk_gib * 1024 ** 3
        for available_memory, available_disk, accepted in ((memory, disk, True), (memory - 1, disk, False), (memory, disk - 1, False)):
            runner = SimulatedRunner([])
            runner.available_memory, runner.available_disk = available_memory, available_disk
            if accepted:
                guest.check_capacity(runner, self.settings, self.root)
            else:
                with self.assertRaisesRegex(guest.GuestError, "no-room"):
                    guest.check_capacity(runner, self.settings, self.root)
        with mock.patch("guest.os.open", side_effect=PermissionError):
            with self.assertRaisesRegex(guest.GuestError, "no-kvm"):
                guest.Runner(10).kvm()

    def test_deadline_settings_private_paths_and_existing_destination(self):
        with self.assertRaises(guest.GuestError):
            guest.Settings(0, 32, 600, 10)
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(guest.GuestError):
                guest.Settings.from_environment()
        runner = SimulatedRunner([])
        deadline = guest.Deadline(5, runner.clock)
        self.assertEqual(deadline.remaining(), 5)
        runner.now += 5
        with self.assertRaisesRegex(guest.GuestError, "expired"):
            deadline.remaining()
        with self.assertRaisesRegex(guest.GuestError, "work-overlaps-cache"):
            with guest.scratch(self.image.parent, self.image):
                self.fail("cache used as scratch")
        self.work.mkdir(mode=0o755)
        self.work.chmod(0o755)
        with self.assertRaisesRegex(guest.GuestError, "work-not-private"):
            self.prepare(SimulatedRunner(["prepared"]))
        self.image.parent.mkdir()
        self.image.write_bytes(IMAGE)
        with self.assertRaisesRegex(guest.GuestError, "destination-exists"):
            self.prepare(SimulatedRunner(["prepared"]))
        self.assertEqual(self.image.read_bytes(), IMAGE)

    def test_event_licensing_and_attestation_types(self):
        for changes, accepted in (({"license_status": 1, "grace_minutes": 0}, True),
                                  ({"license_status": 2, "grace_minutes": 1}, True),
                                  ({"license_status": True}, False),
                                  ({"grace_minutes": "1"}, False),
                                  ({"license_status": 5}, False),
                                  ({"schema": 2}, False),
                                  ({"schema": True}, False),
                                  ({"edition": None}, False),
                                  ({"setup_complete": 1}, False)):
            runner = SimulatedRunner(["ready"])
            runner.seeds = [{"metadata": {"nonce": "n"}}]
            runner.event_changes = changes
            channel = Events(runner, self.root)
            deadline = guest.Deadline(10, runner.clock)
            if accepted:
                self.assertEqual(guest.event(channel, deadline, "ready", "n")["phase"], "ready")
            else:
                with self.assertRaises(guest.GuestError):
                    guest.event(channel, deadline, "ready", "n")

    def test_signal_handlers_are_restored(self):
        before = {s: signal.getsignal(s) for s in (signal.SIGINT, signal.SIGTERM)}
        with self.assertRaisesRegex(guest.GuestError, "interrupted"):
            with guest.cancellation():
                signal.raise_signal(signal.SIGTERM)
        self.assertEqual(before, {s: signal.getsignal(s) for s in before})

    def test_signal_after_process_creation_still_reaps(self):
        for signum in (signal.SIGINT, signal.SIGTERM):
            with self.subTest(signum=signum):
                runner = SimulatedRunner(["prepared"])
                runner.signal_after_popen = signum
                with self.assertRaisesRegex(guest.GuestError, "interrupted"):
                    self.prepare(runner)
                self.assertEqual(len(runner.processes), 1)
                self.assertFalse(self.image.exists())
                self.assert_clean(runner)

    def test_signal_during_directory_removal_does_not_stop_cleanup(self):
        for signum in (signal.SIGINT, signal.SIGTERM):
            with self.subTest(signum=signum):
                runner = SimulatedRunner(["ready", "stopping"])
                remove = guest.shutil.rmtree
                signalled = []

                def removal(*args, **kwargs):
                    signalled.append(True)
                    self.assertFalse(guest._CANCELLATION.get()["interruptible"])
                    signal.raise_signal(signum)
                    return remove(*args, **kwargs)

                with mock.patch("guest.shutil.rmtree", side_effect=removal):
                    with self.assertRaisesRegex(guest.GuestError, "interrupted"):
                        self.verify(runner)
                self.assertTrue(signalled)
                self.assert_clean(runner)
                self.assertEqual(self.image.read_bytes(), IMAGE)

    def test_cleanup_failure_keeps_the_primary_failure(self):
        for operation, phases, failure, expected_stage in (
                (self.prepare, ["prepared"], "wait", "prepare"),
                (self.verify, ["ready", "stopping"], "ssh", "ssh")):
            runner = SimulatedRunner(phases)
            runner.failure = failure
            runner.reap_timeout = True
            with self.assertRaises(guest.GuestError) as error:
                operation(runner)
            self.assertEqual(error.exception.stage, expected_stage)
            self.assertEqual(error.exception.code, "command-failed")
            self.assertEqual(error.exception.cleanup, ("timeout",))
            self.assertNotIn("SECRET", str(error.exception))
            self.assert_clean(runner)

    def test_signal_interrupts_the_guest_event_wait_before_its_deadline(self):
        class WaitingChannel(guest.Channel):
            def __init__(self, directory):
                super().__init__(directory)
                self.closed = False
                self.write_fd = None

            def attach(self, process):
                self.pidfd, self.write_fd = os.pipe()

            def __exit__(self, *args):
                try:
                    super().__exit__(*args)
                finally:
                    if self.write_fd is not None:
                        os.close(self.write_fd)
                    self.closed = True

        for signum in (signal.SIGINT, signal.SIGTERM):
            with self.subTest(signum=signum):
                runner = SimulatedRunner([])
                started = runner.clock()
                waiting = []

                def channel(directory):
                    value = WaitingChannel(directory)
                    runner.channels.append(value)
                    return value

                def select_and_signal(readers, writers, errors, timeout):
                    waiting.append(timeout)
                    self.assertTrue(guest._CANCELLATION.get()["interruptible"])
                    self.assertGreater(timeout, 0)
                    signal.raise_signal(signum)
                    self.fail("the event wait continued after cancellation")

                runner.channel = channel
                with mock.patch("guest.select.select", side_effect=select_and_signal):
                    with self.assertRaisesRegex(guest.GuestError, "interrupted"):
                        self.verify(runner)
                self.assertEqual(len(waiting), 1)
                self.assertLess(runner.clock() - started, self.settings.timeout_seconds)
                self.assert_clean(runner)
                self.assertEqual(self.image.read_bytes(), IMAGE)

    def test_signal_interrupts_the_qemu_exit_wait(self):
        for operation, phases in (("prepare", ["prepared"]), ("verify", ["ready", "stopping"])):
            for signum in (signal.SIGINT, signal.SIGTERM):
                with self.subTest(operation=operation, signum=signum):
                    runner = SimulatedRunner(phases)
                    inject = runner.inject
                    remove = guest.shutil.rmtree
                    removals = []

                    def inject_and_signal(name):
                        if name == "wait":
                            self.assertTrue(guest._CANCELLATION.get()["interruptible"])
                            signal.raise_signal(signum)
                            self.fail("the QEMU exit wait continued after cancellation")
                        inject(name)

                    def removal(*args, **kwargs):
                        # After the interrupted wait, a second signal is only recorded.
                        removals.append(True)
                        self.assertFalse(guest._CANCELLATION.get()["interruptible"])
                        signal.raise_signal(signum)
                        return remove(*args, **kwargs)

                    runner.inject = inject_and_signal
                    with mock.patch("guest.shutil.rmtree", side_effect=removal):
                        with self.assertRaises(guest.GuestError) as error:
                            getattr(self, operation)(runner)
                    self.assertEqual((error.exception.stage, error.exception.code), ("interrupted", "signal"))
                    self.assertEqual(error.exception.cleanup, ())
                    self.assertEqual(len(runner.processes), 1)
                    self.assertEqual(len(runner.processes[0].waits), 1)
                    self.assertGreater(runner.processes[0].waits[0], 0)
                    self.assertTrue(removals)
                    self.assert_clean(runner)
                    if operation == "prepare":
                        self.assertFalse(self.image.exists())
                    else:
                        self.assertEqual(self.image.read_bytes(), IMAGE)

    def test_runner_protects_creation_and_interrupts_communication(self):
        for signum in (signal.SIGINT, signal.SIGTERM):
            for point in ("creation", "communication"):
                with self.subTest(signum=signum, point=point):
                    process = mock.Mock(pid=123)
                    process.poll.return_value = None
                    process.wait.return_value = -9

                    def spawn(*args, **kwargs):
                        self.assertFalse(guest._CANCELLATION.get()["interruptible"])
                        if point == "creation":
                            signal.raise_signal(signum)
                        return process

                    def communicate(timeout):
                        self.assertTrue(guest._CANCELLATION.get()["interruptible"])
                        signal.raise_signal(signum)
                        self.fail("the subprocess wait continued after cancellation")

                    process.communicate.side_effect = communicate
                    with mock.patch("guest.subprocess.Popen", side_effect=spawn) as popen, mock.patch("guest.os.killpg") as kill:
                        with self.assertRaisesRegex(guest.GuestError, "interrupted"):
                            with guest.cancellation():
                                guest.Runner(10).run(["fixture-command"], 600)
                    self.assertTrue(popen.call_args.kwargs["start_new_session"])
                    self.assertEqual(process.communicate.call_count, int(point == "communication"))
                    kill.assert_called_once_with(123, signal.SIGKILL)
                    process.wait.assert_called_once_with(timeout=10)
                    process.stdout.close.assert_called_once()

    def test_runner_results_and_timeout_reap_and_close_output(self):
        for outcome in (0, 7, "timeout"):
            with self.subTest(outcome=outcome):
                process = mock.Mock(pid=123, returncode=outcome)
                process.poll.return_value = None if outcome == "timeout" else outcome
                process.wait.return_value = outcome
                if outcome == "timeout":
                    process.communicate.side_effect = subprocess.TimeoutExpired(["fixture-command"], 600)
                else:
                    process.communicate.return_value = (b"result", None)
                with mock.patch("guest.subprocess.Popen", return_value=process), mock.patch("guest.os.killpg") as kill:
                    if outcome == 0:
                        self.assertEqual(guest.Runner(10).run(["fixture-command"], 600), b"result")
                    else:
                        with self.assertRaises(guest.GuestError) as error:
                            with guest.stage("fixture"):
                                guest.Runner(10).run(["fixture-command"], 600)
                        self.assertEqual(error.exception.code, "timeout" if outcome == "timeout" else "command-failed")
                self.assertEqual(kill.call_count, int(outcome == "timeout"))
                process.wait.assert_called_once_with(timeout=10)
                process.stdout.close.assert_called_once()

    def test_pending_signal_precedes_a_failed_child_exit(self):
        process = mock.Mock(pid=123, returncode=1)
        process.poll.return_value = 1

        def communicate(timeout):
            guest._CANCELLATION.get()["requested"] = True
            return b"discarded", None

        process.communicate.side_effect = communicate
        with mock.patch("guest.subprocess.Popen", return_value=process):
            with self.assertRaisesRegex(guest.GuestError, "interrupted"):
                with guest.cancellation():
                    guest.Runner(10).run(["fixture-command"], 600)
        process.wait.assert_called_once_with(timeout=10)
        process.stdout.close.assert_called_once()

    def test_real_reaper_kills_process_group_and_waits(self):
        process = mock.Mock(pid=123)
        process.poll.return_value = None
        with mock.patch("guest.os.killpg") as kill:
            guest.Runner(10).reap(process, 10)
        kill.assert_called_once_with(123, signal.SIGKILL)
        process.wait.assert_called_once_with(timeout=10)

    def test_serial_json_channel_and_invalid_json(self):
        for payload, valid in ((b'{"phase":"ready"}\r\n', True), (b'SECRET\n', False)):
            with self.subTest(valid=valid):
                with guest.Channel(self.root) as channel:
                    read_fd, write_fd = os.pipe()
                    channel.pidfd = read_fd
                    try:
                        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                            client.connect(str(channel.path))
                            client.sendall(payload)
                            deadline = guest.Deadline(10, lambda: 0)
                            if valid:
                                self.assertEqual(channel.receive(deadline), {"phase": "ready"})
                            else:
                                with self.assertRaisesRegex(guest.GuestError, "invalid-json") as error:
                                    channel.receive(deadline)
                                self.assertNotIn("SECRET", str(error.exception))
                    finally:
                        os.close(write_fd)
                self.assertFalse(channel.path.exists())

    def test_serial_exit_before_a_record_and_deadline(self):
        for process_exited in (True, False):
            with self.subTest(process_exited=process_exited):
                with guest.Channel(self.root) as channel:
                    read_fd, write_fd = os.pipe()
                    channel.pidfd = read_fd
                    deadline = guest.Deadline(10, lambda: 0)
                    try:
                        if process_exited:
                            os.write(write_fd, b"exit")
                            with self.assertRaisesRegex(guest.GuestError, "process-exited-before-record"):
                                channel.receive(deadline)
                        else:
                            with mock.patch("guest.select.select", return_value=([], [], [])) as wait:
                                with self.assertRaisesRegex(guest.GuestError, "deadline"):
                                    channel.receive(deadline)
                                self.assertEqual(wait.call_args.args[-1], 10)
                    finally:
                        os.close(write_fd)
                self.assertFalse(channel.path.exists())

    def test_windows_configuration_is_secret_free_until_disposable_boot(self):
        paths = guest.preparation_inputs()
        self.assertIn("bench/probe/guest.py", paths)
        self.assertIn("bench/probe/windows/source.json", paths)
        self.assertIn("bench/probe/windows/boot.ps1", paths)
        xml = ET.parse(guest.WINDOWS / "autounattend.xml")
        ns = {"u": "urn:schemas-microsoft-com:unattend"}
        self.assertIsNone(xml.find(".//u:AutoLogon", ns))
        password = xml.find(".//u:AdministratorPassword/u:Value", ns)
        self.assertEqual(password.text, "__ADMIN_PASSWORD__")
        finish = (guest.WINDOWS / "finish.ps1").read_text()
        self.assertLess(finish.index("Set-LocalUser -Password"), finish.index("Assert-Base"))
        self.assertLess(finish.index("Disable-LocalUser"), finish.index("Write-BenchEvent 'prepared'"))
        self.assertIn("Wait-Event -SourceIdentifier", finish)
        self.assertNotIn("Start-Sleep", finish)
        specialize = (guest.WINDOWS / "specialize.ps1").read_text()
        self.assertIn("Set-Service sshd -StartupType Disabled", specialize)
        self.assertNotIn("Start-Service", specialize)
        config = (guest.WINDOWS / "sshd_config").read_text()
        for setting in ("AuthenticationMethods publickey", "PasswordAuthentication no", "KbdInteractiveAuthentication no", "AllowUsers benchprobe"):
            self.assertIn(setting, config)
        boot = (guest.WINDOWS / "boot.ps1").read_text()
        self.assertLess(boot.index("Assert-Base"), boot.index("New-LocalUser"))
        self.assertLess(boot.index("Protect-SSHFile"), boot.index("Start-Service sshd"))
        self.assertIn("-RemoteAddress '10.0.2.2'", boot)
        self.assertNotIn("Set-NetFirewallProfile", boot)
        scripts = "\n".join(p.read_text() for p in guest.WINDOWS.glob("*.ps1"))
        self.assertNotIn("slmgr", scripts.lower())
        self.assertNotIn("sysprep", scripts.lower().replace("c:\\windows\\system32\\sysprep\\unattend.xml", ""))
        self.assertNotIn("ssh-keygen", scripts)


if __name__ == "__main__":
    unittest.main()
