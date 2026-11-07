# Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

"""Branch-only Windows probe CLI. Run on the main thread; see README.md."""

import argparse
import contextlib
import dataclasses
import json
import os
import pathlib
import re
import stat
import sys
import tempfile
import urllib.parse

import cache
import guest

ROOT = pathlib.Path(__file__).resolve().parents[2]
COMPRESSION = "qcow2 compressed clusters, zlib (qemu-img convert -c)"
ATTESTATIONS = ("setup_complete", "password_invalidated", "base_accounts_disabled", "base_host_keys_absent")


class ProbeError(ValueError):
    pass


def fields(value, expected):
    if type(value) is not dict or set(value) != set(expected):
        raise ProbeError("record-fields")
    return value


def inputs():
    paths = {"bench/probe/probe.py", "bench/probe/cache.py", *guest.preparation_inputs()}
    result = {}
    for name in sorted(paths):
        path = ROOT / name
        if path.is_symlink() or not path.is_file() or ROOT not in path.resolve().parents:
            raise ProbeError("preparation-input")
        result[name] = path.read_bytes()
    settings = dataclasses.asdict(guest.Settings.from_environment())
    result["bench/probe/runtime-settings.json"] = json.dumps(settings, sort_keys=True).encode("ascii")
    return result


def current_identity():
    return cache.identity(guest.image_source(), os.environ.get("BENCH_STARTED_AT"), inputs())


def require_kvm():
    if not os.path.exists("/dev/kvm") or not os.access("/dev/kvm", os.W_OK):
        raise guest.GuestError("kvm", "no-kvm")
    try:
        descriptor = os.open("/dev/kvm", os.O_RDWR)
    except OSError:
        raise guest.GuestError("kvm", "no-kvm") from None
    os.close(descriptor)


def disjoint(left, right):
    left, right = pathlib.Path(left).resolve(), pathlib.Path(right).resolve()
    if left == right or left in right.parents or right in left.parents:
        raise ProbeError("directories-overlap")


@contextlib.contextmanager
def private_work(image_dir, result_dir):
    temporary = tempfile.TemporaryDirectory(prefix="bench-private-", dir=os.environ.get("RUNNER_TEMP"))
    with guest.cleanup_scope(temporary.cleanup):
        path = pathlib.Path(temporary.name)
        disjoint(path, image_dir)
        disjoint(path, result_dir)
        yield path


def unique_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ProbeError("duplicate-field")
        value[key] = item
    return value


def reject_constant(value):
    raise ProbeError("nonfinite-json")


def read_json(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "r", encoding="utf-8") as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise ProbeError("metadata-not-regular")
        return json.load(source, object_pairs_hook=unique_object, parse_constant=reject_constant)


def clean_guest(value, phase):
    fields(value, ("schema", "phase", *ATTESTATIONS, "edition", "license_status", "grace_minutes"))
    if (type(value["schema"]) is not int or value["schema"] != 1 or value["phase"] != phase
            or any(value[key] is not True for key in ATTESTATIONS)
            or not isinstance(value["edition"], str)
            or re.fullmatch(r"Server(?:Standard|Datacenter)Eval", value["edition"]) is None
            or type(value["license_status"]) is not int or type(value["grace_minutes"]) is not int
            or value["grace_minutes"] < 0
            or not (value["license_status"] == 1 or (value["license_status"] == 2 and value["grace_minutes"] > 0))):
        raise ProbeError("guest-evidence")
    return dict(value)


def clean_source(value, source):
    fields(value, ("requested_url", "final_url", "sha256", "sha256_kind", "publisher_sha256", "bytes"))
    final = urllib.parse.urlsplit(value["final_url"])
    if (value["requested_url"] != source["url"] or final.scheme != "https"
            or final.hostname not in source["download_hosts"] or final.username or final.password
            or final.query or final.fragment or value["sha256_kind"] != "measured"
            or not isinstance(value["sha256"], str) or re.fullmatch(r"[0-9a-f]{64}", value["sha256"]) is None
            or value["publisher_sha256"] != source["publisher_sha256"]
            or (value["publisher_sha256"] is not None and value["sha256"] != value["publisher_sha256"])
            or type(value["bytes"]) is not int or value["bytes"] <= 0):
        raise ProbeError("source-evidence")
    return dict(value)


def clean_preparation(value, expected, material):
    fields(value, ("started_at", "compression", "source_evidence", "guest_evidence"))
    if cache.identity(expected["source"], value["started_at"], material) != expected or value["compression"] != COMPRESSION:
        raise ProbeError("preparation-evidence")
    return {"started_at": value["started_at"], "compression": COMPRESSION,
            "source_evidence": clean_source(value["source_evidence"], expected["source"]),
            "guest_evidence": clean_guest(value["guest_evidence"], "prepared")}


def restored(directory, expected, material):
    envelope = read_json(directory / "prepared.json")
    fields(envelope, ("version", "provenance", "preparation"))
    if type(envelope["version"]) is not int or envelope["version"] != 1:
        raise ProbeError("envelope-version")
    measurements = cache.validate_metadata(envelope["provenance"], expected, directory / "image.qcow2")
    if measurements["compressed_bytes"] != envelope["provenance"]["image"]["bytes"]:
        raise ProbeError("compressed-size")
    preparation = clean_preparation(envelope["preparation"], expected, material)
    return measurements, preparation


def prepare(directory, results, expected, material):
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    if list(directory.iterdir()):
        raise ProbeError("cache-not-empty")
    staging = tempfile.TemporaryDirectory(prefix=".prepare-", dir=directory)
    with guest.cleanup_scope(staging.cleanup):
        stage = pathlib.Path(staging.name)
        image = stage / "image.qcow2"
        with private_work(directory, results) as work:
            measured = guest.prepare_image(image, work)
        provenance = cache.make_metadata(expected, image, measured["preparation_seconds"], measured["compressed_bytes"])
        if provenance["image"]["bytes"] != measured["compressed_bytes"]:
            raise ProbeError("compressed-size")
        evidence = clean_preparation({"started_at": os.environ["BENCH_STARTED_AT"],
            "compression": measured["compression"], "source_evidence": measured["source_evidence"],
            "guest_evidence": measured["guest_evidence"]}, expected, material)
        envelope = {"version": 1, "provenance": provenance, "preparation": evidence}
        with (stage / "prepared.json").open("x", encoding="utf-8") as output:
            os.chmod(output.name, 0o600)
            json.dump(envelope, output, allow_nan=False, sort_keys=True)
            output.flush()
            os.fsync(output.fileno())
        guest.check_cancellation()
        # Publish complete files without replacing an existing cache entry.
        os.link(image, directory / "image.qcow2")
        try:
            os.link(stage / "prepared.json", directory / "prepared.json")
        except BaseException:
            (directory / "image.qcow2").unlink()
            raise
    return dict(provenance["measurements"], preparation=evidence)


def verify(directory, results, expected, material):
    measurements, preparation = restored(directory, expected, material)
    with private_work(directory, results) as work:
        evidence = clean_guest(guest.verify_image(directory / "image.qcow2", work), "ready")
    if evidence["edition"] != preparation["guest_evidence"]["edition"]:
        raise ProbeError("changed-edition")
    return dict(measurements, preparation=preparation, guest_evidence=evidence)


def append_result(directory, value):
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    descriptor = os.open(directory / "probe.jsonl", os.O_WRONLY | os.O_CREAT | os.O_APPEND | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    with os.fdopen(descriptor, "a", encoding="utf-8") as output:
        if not stat.S_ISREG(os.fstat(output.fileno()).st_mode):
            raise ProbeError("result-not-regular")
        output.write(json.dumps(value, allow_nan=False, sort_keys=True) + "\n")
        output.flush()
        os.fsync(output.fileno())


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="command", required=True)
    commands.add_parser("identity").add_argument("--github-output", type=pathlib.Path, required=True)
    for name in ("kvm", "prepare", "verify"):
        command = commands.add_parser(name)
        command.add_argument("--result-dir", type=pathlib.Path, required=True)
        if name != "kvm":
            command.add_argument("--image-dir", type=pathlib.Path, required=True)
    return result


def main(argv=None):
    args = parser().parse_args(argv)
    if args.command == "identity":
        try:
            identity = current_identity()
            with args.github_output.open("a", encoding="utf-8") as output:
                output.write("cache_key=" + identity["cache_key"] + "\n")
            return 0
        except Exception:
            print("bench: identity-failed", file=sys.stderr)
            return 1
    record = {"event": "probe-" + args.command, "status": "failed", "guest_evidence": None,
              "installation_time": None, "installation_time_status": "unavailable"}
    try:
        with guest.cancellation():
            require_kvm()
            if args.command == "kvm":
                record["status"] = "kvm-available"
            else:
                disjoint(args.image_dir, args.result_dir)
                material = inputs()
                expected = cache.identity(guest.image_source(), os.environ.get("BENCH_STARTED_AT"), material)
                record.update(started_at=os.environ["BENCH_STARTED_AT"], cache_key=expected["cache_key"])
                operation = prepare if args.command == "prepare" else verify
                record.update(operation(args.image_dir.resolve(), args.result_dir.resolve(), expected, material))
                record["status"] = "prepared" if args.command == "prepare" else "ok"
    except guest.GuestError as error:
        record.update(status="no-kvm" if error.code == "no-kvm" else args.command + "-failed",
                      stage=error.stage, code=error.code, cleanup=list(error.cleanup))
    except KeyboardInterrupt:
        record.update(status=args.command + "-failed", stage="interrupted", code="signal")
    except Exception as error:
        record.update(status=args.command + "-failed", stage="cache" if isinstance(error, (cache.CacheError, ProbeError)) else "host",
                      code="invalid-cache" if isinstance(error, (cache.CacheError, ProbeError)) else "dependency-failed",
                      cleanup=list(getattr(error, "cleanup", ())))
    try:
        append_result(args.result_dir, record)
    except Exception:
        print("bench: result-failed", file=sys.stderr)
        return 1
    line = "bench: " + record["status"]
    print(line)
    return 0 if record["status"] in ("ok", "prepared", "kvm-available") else 1


if __name__ == "__main__":
    sys.exit(main())
