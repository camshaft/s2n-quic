# Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

"""A Windows evaluation guest for the branch-only bench probe.

The caller supplies a private work directory, outside both its cache and its
uploaded results. Only image.qcow2 and the caller's prepared.json may be cached.
BENCH_MEMORY_MIB, BENCH_DISK_GIB, BENCH_TIMEOUT_SECONDS and
BENCH_CLEANUP_SECONDS are required positive integers chosen by the job owner.
The operation deadline includes download, installation and compression; cleanup
has its separate configured allowance. The job must allow both allowances.

Tests inject Runner at the process and event boundaries. They do not establish
that Windows installed or booted. See windows/README.md for the live checks.
"""

import contextlib
import contextvars
import dataclasses
import hashlib
import json
import math
import os
import pathlib
import secrets
import select
import shutil
import signal
import socket
import stat
import subprocess
import tempfile
import time
import urllib.parse
import xml.etree.ElementTree as ET
from xml.sax.saxutils import escape

ROOT = pathlib.Path(__file__).resolve().parent
WINDOWS = ROOT / "windows"
_CANCELLATION = contextvars.ContextVar("bench_cancellation", default=None)


class GuestError(RuntimeError):
    """A stage and a closed error code; never subprocess output or credentials."""

    def __init__(self, stage, code, cleanup=()):
        self.stage = stage
        self.code = code
        self.cleanup = tuple(cleanup)
        super().__init__(f"{stage}: {code}")


def image_source():
    return json.loads((WINDOWS / "source.json").read_text(encoding="utf-8"))


def preparation_inputs():
    """Repository-relative paths, including this backend, in digest order."""
    return ["bench/probe/guest.py"] + [
        "bench/probe/windows/" + path.name
        for path in sorted(WINDOWS.iterdir()) if path.is_file()
    ]


@dataclasses.dataclass(frozen=True)
class Settings:
    memory_mib: int
    disk_gib: int
    timeout_seconds: int
    cleanup_seconds: int

    def __post_init__(self):
        for value in dataclasses.astuple(self):
            if type(value) is not int or value <= 0:
                raise GuestError("configuration", "positive-integers-required")

    @classmethod
    def from_environment(cls):
        try:
            return cls(*(int(os.environ["BENCH_" + field]) for field in (
                "MEMORY_MIB", "DISK_GIB", "TIMEOUT_SECONDS", "CLEANUP_SECONDS"
            )))
        except (KeyError, ValueError):
            raise GuestError("configuration", "required-settings") from None


class Deadline:
    def __init__(self, seconds, clock):
        self.clock = clock
        self.end = clock() + seconds

    def remaining(self):
        check_cancellation()
        left = self.end - self.clock()
        if left <= 0 or not math.isfinite(left):
            raise GuestError("deadline", "expired")
        return left


class Channel:
    """Read the guest's explicit JSON events over a private serial socket.

    Linux pidfds make an early QEMU exit an event too. The listener exists before
    QEMU starts. There is one connection because this object owns one guest.
    """

    def __init__(self, directory):
        self.path = directory / "serial.sock"
        self.server = None
        self.connection = None
        self.pidfd = None
        self.buffer = b""

    def __enter__(self):
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            self.server.bind(str(self.path))
            self.server.listen(1)
        except BaseException:
            self.server.close()
            self.path.unlink(missing_ok=True)
            raise
        return self

    def attach(self, process):
        self.pidfd = os.pidfd_open(process.pid)

    def receive(self, deadline):
        while b"\n" not in self.buffer:
            stream = self.connection or self.server
            readable, _, _ = select.select([stream, self.pidfd], [], [], deadline.remaining())
            if not readable:
                raise GuestError("event", "deadline")
            if stream in readable:
                if self.connection is None:
                    self.connection, _ = self.server.accept()
                else:
                    chunk = self.connection.recv(8192)
                    if not chunk:
                        raise GuestError("event", "closed-before-record")
                    self.buffer += chunk
            elif self.pidfd in readable:
                raise GuestError("event", "process-exited-before-record")
        line, self.buffer = self.buffer.split(b"\n", 1)
        try:
            record = json.loads(line.decode("ascii"))
        except (ValueError, UnicodeError):
            raise GuestError("event", "invalid-json") from None
        if not isinstance(record, dict):
            raise GuestError("event", "invalid-record")
        return record

    def __exit__(self, *_):
        if self.connection is not None:
            self.connection.close()
        if self.server is not None:
            self.server.close()
        if self.pidfd is not None:
            os.close(self.pidfd)
        self.path.unlink(missing_ok=True)


class Runner:
    clock = staticmethod(time.monotonic)

    def kvm(self):
        try:
            fd = os.open("/dev/kvm", os.O_RDWR)
        except OSError:
            raise GuestError("kvm", "no-kvm") from None
        os.close(fd)

    def memory_bytes(self):
        # The kernel exports MemAvailable in KiB in this documented interface.
        fields = dict(line.split(":", 1) for line in pathlib.Path("/proc/meminfo").read_text().splitlines())
        return int(fields["MemAvailable"].split()[0]) * 1024

    def disk_bytes(self, path):
        return shutil.disk_usage(path).free

    def run(self, command, timeout):
        result = subprocess.run(command, check=True, stdout=subprocess.PIPE,
                                stderr=subprocess.DEVNULL, timeout=timeout)
        check_cancellation()
        return result.stdout

    def popen(self, command):
        return subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL, start_new_session=True)

    def channel(self, directory):
        return Channel(directory)

    def reap(self, process, timeout):
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        process.wait(timeout=timeout)


@contextlib.contextmanager
def stage(name):
    try:
        yield
    except GuestError:
        raise
    except subprocess.TimeoutExpired as error:
        raise GuestError(name, "timeout", getattr(error, "cleanup", ())) from None
    except subprocess.CalledProcessError as error:
        raise GuestError(name, "command-failed", getattr(error, "cleanup", ())) from None
    except (OSError, ValueError, KeyError, ET.ParseError) as error:
        raise GuestError(name, "dependency-failed", getattr(error, "cleanup", ())) from None


def check_cancellation():
    state = _CANCELLATION.get()
    if state is not None and state["requested"]:
        raise GuestError("interrupted", "signal")


@contextlib.contextmanager
def cancellation():
    """Remember signals; raise only at checkpoints outside acquisition and cleanup.

    A running dependency can finish or reach its configured timeout before the
    next checkpoint. A signal during cleanup is reported after cleanup finishes.
    """
    previous = {}
    state = {"requested": False}
    token = _CANCELLATION.set(state)

    def interrupted(signum, frame):
        state["requested"] = True

    try:
        for signum in (signal.SIGINT, signal.SIGTERM):
            previous[signum] = signal.signal(signum, interrupted)
        yield
        check_cancellation()
    finally:
        for signum, handler in previous.items():
            signal.signal(signum, handler)
        _CANCELLATION.reset(token)


@contextlib.contextmanager
def cleanup_scope(clean):
    """Preserve a primary failure and append only closed cleanup error codes."""
    failure = None
    try:
        yield
    except BaseException as error:
        failure = error
        raise
    finally:
        try:
            clean()
        except BaseException as error:
            if isinstance(error, subprocess.TimeoutExpired):
                code = "timeout"
            elif isinstance(error, subprocess.CalledProcessError):
                code = "command-failed"
            elif isinstance(error, KeyboardInterrupt):
                code = "interrupted"
            else:
                code = "dependency-failed"
            if failure is None:
                raise GuestError("cleanup", code) from None
            failure.cleanup = (*getattr(failure, "cleanup", ()), code)


def private_write(path, value):
    data = value.encode("utf-8") if isinstance(value, str) else value
    with open(path, "xb") as output:
        os.chmod(path, 0o600)
        output.write(data)


@contextlib.contextmanager
def scratch(work_dir, image_path):
    work = pathlib.Path(work_dir).resolve()
    image = pathlib.Path(image_path).resolve()
    if work == image.parent or work in image.parents or image.parent in work.parents:
        raise GuestError("configuration", "work-overlaps-cache")
    work.mkdir(parents=True, mode=0o700, exist_ok=True)
    info = work.stat()
    if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) & 0o077:
        raise GuestError("configuration", "work-not-private")
    temporary = tempfile.TemporaryDirectory(prefix="guest-", dir=work)
    with cleanup_scope(temporary.cleanup):
        yield pathlib.Path(temporary.name)


def check_capacity(runner, settings, directory):
    if runner.memory_bytes() < settings.memory_mib * 1024 * 1024:
        raise GuestError("capacity", "no-room")
    # Reserve space for both the full virtual disk and a compressed conversion.
    if runner.disk_bytes(directory) < 2 * settings.disk_gib * 1024 ** 3:
        raise GuestError("capacity", "no-room")


def image_record(xml, source):
    root = ET.fromstring(xml)
    matches = [item for item in root.findall("IMAGE")
               if item.findtext("DISPLAYNAME") == source["display_name"]
               and item.findtext("WINDOWS/INSTALLATIONTYPE") == "Server Core"
               and item.findtext("WINDOWS/ARCH") == "9"]
    if len(matches) != 1:
        raise GuestError("source", "edition-not-unique")
    item = matches[0]
    index = item.get("INDEX", "")
    edition = item.findtext("WINDOWS/EDITIONID", "")
    if not index.isdecimal() or int(index) < 1 or not edition.endswith("Eval"):
        raise GuestError("source", "not-evaluation")
    return {"index": int(index), "edition": edition}


def download(runner, deadline, directory, source):
    iso = directory / "windows.iso"
    raw = runner.run(["curl", "--fail", "--silent", "--show-error", "--location",
                      "--proto", "=https", "--proto-redir", "=https",
                      "--max-time", str(deadline.remaining()), "--output", str(iso),
                      "--write-out", "%{json}", source["url"]], deadline.remaining())
    result = json.loads(raw)
    final = urllib.parse.urlsplit(result["url_effective"])
    if (final.scheme != "https" or final.hostname not in source["download_hosts"]
            or final.username or final.password or final.query or final.fragment
            or result["ssl_verify_result"] != 0):
        raise GuestError("source", "untrusted-download")
    digest = hashlib.sha256()
    with iso.open("rb") as data:
        for block in iter(lambda: data.read(1024 * 1024), b""):
            deadline.remaining()
            digest.update(block)
    if iso.stat().st_size == 0 or iso.stat().st_size != int(result["size_download"]):
        raise GuestError("source", "download-size")
    measured = digest.hexdigest()
    expected = source["publisher_sha256"]
    if expected is not None and measured != expected:
        raise GuestError("source", "publisher-digest-mismatch")
    return iso, {"requested_url": source["url"], "final_url": result["url_effective"],
                 "sha256": measured, "sha256_kind": "measured",
                 "publisher_sha256": expected, "bytes": iso.stat().st_size}


def make_seed(runner, deadline, directory, label, metadata, files):
    tree = directory / "seed"
    tree.mkdir(mode=0o700)
    private_write(tree / "seed.json", json.dumps(metadata))
    private_write(tree / (label.lower() + ".tag"), "")
    for path, data in files.items():
        target = tree / path
        target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        private_write(target, data)
    seed = directory / "seed.iso"
    runner.run(["genisoimage", "-quiet", "-J", "-R", "-V", label,
                "-o", str(seed), str(tree)], deadline.remaining())
    os.chmod(seed, 0o600)
    return seed


def qemu_command(settings, disk, seed, serial_path, iso=None, port=None):
    def drive(path, options):
        return "file=" + str(path).replace(",", ",,") + "," + options
    command = ["qemu-system-x86_64", "-accel", "kvm", "-cpu", "host",
               "-m", str(settings.memory_mib), "-display", "none", "-monitor", "none",
               "-chardev", "socket,id=events,path=" + str(serial_path).replace(",", ",,"),
               "-serial", "chardev:events", "-rtc", "base=utc",
               "-drive", drive(disk, "if=ide,index=0,format=qcow2")]
    if iso is not None:
        command += ["-drive", drive(iso, "if=ide,index=2,media=cdrom,readonly=on"),
                    "-boot", "order=c,once=d"]
    command += ["-drive", drive(seed, "if=ide,index=3,media=cdrom,readonly=on")]
    if port is None:
        command += ["-nic", "none"]
    else:
        command += ["-netdev", f"user,id=net,restrict=on,hostfwd=tcp:127.0.0.1:{port}-:22",
                    "-device", "e1000,netdev=net"]
    return command


@contextlib.contextmanager
def virtual_machine(runner, deadline, settings, directory, disk, seed, iso=None, port=None):
    channel = runner.channel(directory)
    with cleanup_scope(lambda: channel.__exit__(None, None, None)):
        channel.__enter__()
        process = None

        def reap():
            if process is not None:
                runner.reap(process, settings.cleanup_seconds)

        with cleanup_scope(reap):
            process = runner.popen(qemu_command(settings, disk, seed, channel.path, iso, port))
            deadline.remaining()
            channel.attach(process)
            yield process, channel


def event(channel, deadline, phase, nonce, edition=None):
    record = channel.receive(deadline)
    if type(record.get("schema")) is not int or record["schema"] != 1 or record.get("nonce") != nonce:
        raise GuestError("event", "wrong-identity")
    if record.get("phase") != phase:
        raise GuestError("event", "unexpected-phase")
    if phase in ("prepared", "ready"):
        required = ("setup_complete", "password_invalidated", "base_accounts_disabled", "base_host_keys_absent")
        if any(record.get(key) is not True for key in required):
            raise GuestError("event", "missing-attestation")
        if not isinstance(record.get("edition"), str) or not record["edition"]:
            raise GuestError("event", "wrong-edition")
        if edition is not None and record.get("edition") != edition:
            raise GuestError("event", "wrong-edition")
        status, grace = record.get("license_status"), record.get("grace_minutes")
        if (type(status) is not int or type(grace) is not int or grace < 0
                or not (status == 1 or (status == 2 and grace > 0))):
            raise GuestError("license", "not-usable")
        # Export declared evidence only, not arbitrary guest text or protocol tokens.
        return {key: record[key] for key in (
            "schema", "phase", *required, "edition", "license_status", "grace_minutes"
        )}
    return {"schema": 1, "phase": phase}


def clean_exit(process, deadline):
    if process.wait(timeout=deadline.remaining()) != 0:
        raise GuestError("shutdown", "qemu-failed")
    check_cancellation()


def prepare_image(image_path, work_dir, *, settings=None, runner=None):
    settings, runner = settings or Settings.from_environment(), runner or Runner()
    with stage("prepare"), cancellation():
        runner.kvm()
        deadline = Deadline(settings.timeout_seconds, runner.clock)
        started = runner.clock()
        image = pathlib.Path(image_path).resolve()
        if image.exists():
            raise GuestError("prepare", "destination-exists")
        with scratch(work_dir, image) as directory:
            check_capacity(runner, settings, directory)
            source = image_source()
            iso, evidence = download(runner, deadline, directory, source)
            runner.run(["7z", "e", "-y", "-o" + str(directory), str(iso), "sources/install.wim"], deadline.remaining())
            selected = image_record(runner.run(["wimlib-imagex", "info", str(directory / "install.wim"), "--xml"], deadline.remaining()), source)
            nonce = secrets.token_hex(16)
            metadata = {"schema": 1, "nonce": nonce, **selected, "source_evidence": evidence}
            # This temporary password is invalidated in finish.ps1 before caching.
            password = "Aa1!" + secrets.token_hex(32)
            answer = (WINDOWS / "autounattend.xml").read_text().replace("__IMAGE_INDEX__", str(selected["index"]))
            answer = answer.replace("__ADMIN_PASSWORD__", escape(password))
            files = {"autounattend.xml": answer, "specialize.cmd": (WINDOWS / "specialize.cmd").read_bytes()}
            for name in ("common.ps1", "specialize.ps1", "finish.ps1", "boot.ps1", "shutdown.ps1", "SetupComplete.cmd", "sshd_config"):
                files["payload/" + name] = (WINDOWS / name).read_bytes()
            seed = make_seed(runner, deadline, directory, "BENCHPREP", metadata, files)
            check_capacity(runner, settings, directory)
            disk = directory / "install.qcow2"
            runner.run(["qemu-img", "create", "-f", "qcow2", str(disk), str(settings.disk_gib * 1024 ** 3)], deadline.remaining())
            with virtual_machine(runner, deadline, settings, directory, disk, seed, iso=iso) as (process, channel):
                prepared = event(channel, deadline, "prepared", nonce, selected["edition"])
                clean_exit(process, deadline)
            compressed = directory / "compressed.qcow2"
            runner.run(["qemu-img", "convert", "-f", "qcow2", "-O", "qcow2", "-c", "-o", "compression_type=zlib", str(disk), str(compressed)], deadline.remaining())
            size = compressed.stat().st_size
            if size <= 0:
                raise GuestError("prepare", "empty-image")
            image.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            # Exclusive creation refuses a concurrent writer; failures remove only our file.
            with image.open("xb") as output:
                try:
                    os.chmod(image, 0o600)
                    with compressed.open("rb") as data:
                        while block := data.read(1024 * 1024):
                            deadline.remaining()
                            output.write(block)
                    output.flush()
                    os.fsync(output.fileno())
                except BaseException:
                    image.unlink(missing_ok=True)
                    raise
            elapsed = float(runner.clock() - started)
        return {"preparation_seconds": elapsed, "compressed_bytes": size,
                "compression": "qcow2 compressed clusters, zlib (qemu-img convert -c)",
                "source_evidence": evidence, "guest_evidence": prepared}


def ssh_command(key, hosts, nonce, port, command):
    options = {"BatchMode": "yes", "IdentitiesOnly": "yes", "IdentityAgent": "none",
               "StrictHostKeyChecking": "yes", "UserKnownHostsFile": str(hosts),
               "GlobalKnownHostsFile": "/dev/null", "HostKeyAlias": "bench-" + nonce,
               "HostKeyAlgorithms": "ssh-ed25519", "PasswordAuthentication": "no",
               "KbdInteractiveAuthentication": "no", "PreferredAuthentications": "publickey",
               "ForwardAgent": "no", "ClearAllForwardings": "yes"}
    args = ["ssh", "-F", "/dev/null", "-i", str(key), "-p", str(port)]
    for name, value in options.items():
        args += ["-o", name + "=" + value]
    return args + ["benchprobe@127.0.0.1", command]


def verify_image(image_path, work_dir, *, settings=None, runner=None):
    settings, runner = settings or Settings.from_environment(), runner or Runner()
    with stage("verify"), cancellation():
        runner.kvm()
        deadline = Deadline(settings.timeout_seconds, runner.clock)
        image = pathlib.Path(image_path).resolve(strict=True)
        with scratch(work_dir, image) as directory:
            check_capacity(runner, settings, directory)
            nonce = secrets.token_hex(16)
            client, host = directory / "client", directory / "host"
            for key in (client, host):
                runner.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "bench", "-f", str(key)], deadline.remaining())
                for key_file in (key, key.with_suffix(".pub")):
                    os.chmod(key_file, 0o600)
            public = runner.run(["ssh-keygen", "-y", "-f", str(host)], deadline.remaining()).decode("ascii").strip()
            fields = public.split()
            if len(fields) < 2 or fields[0] != "ssh-ed25519":
                raise GuestError("keys", "host-public-key")
            hosts = directory / "known_hosts"
            private_write(hosts, "bench-" + nonce + " " + " ".join(fields[:2]) + "\n")
            metadata = {"schema": 1, "nonce": nonce}
            seed = make_seed(runner, deadline, directory, "BENCHRUN", metadata,
                             {"host_key": host.read_bytes(), "client.pub": client.with_suffix(".pub").read_bytes()})
            overlay = directory / "overlay.qcow2"
            runner.run(["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b", str(image), str(overlay)], deadline.remaining())
            # The OS chooses a free loopback port. A competing bind makes QEMU fail closed.
            with socket.socket() as reservation:
                reservation.bind(("127.0.0.1", 0))
                port = reservation.getsockname()[1]
            with virtual_machine(runner, deadline, settings, directory, overlay, seed, port=port) as (process, channel):
                event(channel, deadline, "ready", nonce)
                with stage("ssh"):
                    reply = runner.run(ssh_command(client, hosts, nonce, port,
                        'powershell.exe -NoProfile -NonInteractive -Command "Write-Output BENCH_SSH_OK"'), deadline.remaining())
                    if reply.strip() != b"BENCH_SSH_OK":
                        raise GuestError("ssh", "wrong-command-result")
                    try:
                        runner.run(ssh_command(client, hosts, nonce, port,
                            'powershell.exe -NoProfile -NonInteractive -File C:/BenchProbe/shutdown.ps1'), deadline.remaining())
                    except subprocess.CalledProcessError as error:
                        if error.returncode != 255:
                            raise
                # Shutdown may close SSH first; require its independent event and QEMU exit.
                event(channel, deadline, "stopping", nonce)
                clean_exit(process, deadline)
