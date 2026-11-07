# Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

"""Deterministic tests of bench/probe/cache.py.

Run them from the repository's root:

    python3 -m unittest discover -s bench/probe/tests

They use fixture data and temporary files alone.
"""

import copy
import datetime
import hashlib
import json
import os
import pathlib
import sys
import tempfile
import time
import unittest
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))

import cache  # noqa: E402

SOURCE = {
    "url": "https://example.invalid/windows-server-evaluation.iso",
    "sha256": "a" * 64,
    "edition": "Windows Server Datacenter Evaluation",
    "media": {"language": "en-us", "architecture": "x64"},
    "image_index": 4,
}

SCRIPT = "bench/probe/windows/setup.ps1"

INPUTS = {
    "bench/probe/probe.py": b"print('prepare')\n",
    "bench/probe/windows/answer.xml": b"<answer/>\n",
    SCRIPT: b"Write-Output 'setup'\r\n",
}

STARTED = "2026-11-06T19:47:58.336686+00:00"

IMAGE_BYTES = b"QFI\xfb" + bytes(range(256)) * 16


def reversed_mapping(mapping):
    """Return a dict of mapping's items in reverse order, nested dicts too."""
    return {
        key: reversed_mapping(value) if isinstance(value, dict) else value
        for key, value in reversed(list(mapping.items()))
    }


def with_input(path, content):
    changed = dict(INPUTS)
    changed[path] = content
    return changed


def without_input(path):
    return {name: content for name, content in INPUTS.items() if name != path}


class NoClock(datetime.datetime):
    """A datetime class whose clock reads fail, to show cache.py doesn't read the clock."""

    @classmethod
    def now(cls, tz=None):
        raise AssertionError("cache.py read the clock with datetime.now()")

    @classmethod
    def utcnow(cls):
        raise AssertionError("cache.py read the clock with datetime.utcnow()")

    @classmethod
    def today(cls):
        raise AssertionError("cache.py read the clock with datetime.today()")


class IdentityTest(unittest.TestCase):
    def test_cache_error_is_a_value_error(self):
        self.assertTrue(issubclass(cache.CacheError, ValueError))

    def test_returns_the_four_fields(self):
        result = cache.identity(SOURCE, STARTED, INPUTS)
        self.assertEqual(list(result), ["source", "preparation_digest", "quarter", "cache_key"])
        self.assertEqual(result["source"], SOURCE)
        self.assertIsNot(result["source"], SOURCE)
        self.assertRegex(result["preparation_digest"], r"\A[0-9a-f]{64}\Z")
        self.assertEqual(result["quarter"], "2026-Q4")
        self.assertEqual(cache.CACHE_KEY_PREFIX, "bench-probe-windows-image-v1")
        self.assertRegex(
            result["cache_key"], r"\Abench-probe-windows-image-v1-2026-Q4-[0-9a-f]{64}\Z"
        )
        # GitHub's cache keys hold at most 512 characters and no commas.
        self.assertLessEqual(len(result["cache_key"]), 512)
        self.assertNotIn(",", result["cache_key"])
        self.assertEqual(json.loads(json.dumps(result, allow_nan=False)), result)

    def test_digest_frames_each_path_and_content(self):
        expected = hashlib.sha256(b"s2n-quic bench probe preparation inputs v1\n")
        entries = sorted((path.encode("utf-8"), content) for path, content in INPUTS.items())
        expected.update(len(entries).to_bytes(8, "big"))
        for path, content in entries:
            for part in (path, content):
                expected.update(len(part).to_bytes(8, "big"))
                expected.update(part)
        result = cache.identity(SOURCE, STARTED, INPUTS)
        self.assertEqual(result["preparation_digest"], expected.hexdigest())

    def test_cache_key_hashes_canonical_json(self):
        result = cache.identity(SOURCE, STARTED, INPUTS)
        material = json.dumps(
            {
                "source": SOURCE,
                "preparation_digest": result["preparation_digest"],
                "quarter": "2026-Q4",
            },
            sort_keys=True,
            separators=(",", ":"),
            ensure_ascii=True,
        ).encode("ascii")
        self.assertEqual(
            result["cache_key"],
            "bench-probe-windows-image-v1-2026-Q4-" + hashlib.sha256(material).hexdigest(),
        )

    def test_stable_despite_mapping_order(self):
        source = reversed_mapping(SOURCE)
        inputs = reversed_mapping(INPUTS)
        self.assertNotEqual(list(source), list(SOURCE))
        self.assertNotEqual(list(source["media"]), list(SOURCE["media"]))
        self.assertNotEqual(list(inputs), list(INPUTS))
        first = cache.identity(SOURCE, STARTED, INPUTS)
        second = cache.identity(source, STARTED, inputs)
        self.assertEqual(second, first)

    def test_each_source_change_changes_the_key(self):
        base = cache.identity(SOURCE, STARTED, INPUTS)
        changes = {
            "url": dict(SOURCE, url="https://example.invalid/other.iso"),
            "sha256": dict(SOURCE, sha256="b" * 64),
            "edition": dict(SOURCE, edition="Windows Server Standard Evaluation"),
            "nested value": dict(SOURCE, media={"language": "en-gb", "architecture": "x64"}),
            "added field": dict(SOURCE, build="26100"),
            "removed field": {key: value for key, value in SOURCE.items() if key != "image_index"},
            "int to text": dict(SOURCE, image_index="4"),
            "int to float": dict(SOURCE, image_index=4.0),
            "int to list": dict(SOURCE, image_index=[4]),
        }
        keys = {base["cache_key"]}
        for name, source in changes.items():
            with self.subTest(change=name):
                changed = cache.identity(source, STARTED, INPUTS)
                self.assertEqual(changed["preparation_digest"], base["preparation_digest"])
                self.assertEqual(changed["quarter"], base["quarter"])
                self.assertNotIn(changed["cache_key"], keys)
                keys.add(changed["cache_key"])

    def test_json_types_are_kept_apart(self):
        values = (1, True, 1.0, "1", [1], None)
        keys = {cache.identity({"value": value}, STARTED, INPUTS)["cache_key"] for value in values}
        self.assertEqual(len(keys), len(values))

    def test_each_input_change_changes_the_key(self):
        base = cache.identity(SOURCE, STARTED, INPUTS)
        renamed = without_input(SCRIPT)
        renamed["bench/probe/windows/setup2.ps1"] = INPUTS[SCRIPT]
        changes = {
            "one byte": with_input(SCRIPT, b"Write-Output 'setuq'\r\n"),
            "appended byte": with_input(SCRIPT, INPUTS[SCRIPT] + b"\n"),
            "line ending": with_input(SCRIPT, b"Write-Output 'setup'\n"),
            "emptied": with_input(SCRIPT, b""),
            "renamed": renamed,
            "added path": with_input("bench/probe/windows/extra.ps1", b""),
            "removed path": without_input(SCRIPT),
        }
        keys = {base["cache_key"]}
        digests = {base["preparation_digest"]}
        for name, inputs in changes.items():
            with self.subTest(change=name):
                changed = cache.identity(SOURCE, STARTED, inputs)
                self.assertEqual(changed["quarter"], base["quarter"])
                self.assertNotIn(changed["preparation_digest"], digests)
                self.assertNotIn(changed["cache_key"], keys)
                digests.add(changed["preparation_digest"])
                keys.add(changed["cache_key"])

    def test_paths_and_contents_are_framed(self):
        pairs = [
            ({"a": b"bc"}, {"ab": b"c"}),
            ({"a": b"xy", "b": b""}, {"a": b"x", "b": b"y"}),
            ({"a/b": b""}, {"a": b"/b"}),
        ]
        for first, second in pairs:
            with self.subTest(first=first, second=second):
                self.assertNotEqual(
                    cache.identity(SOURCE, STARTED, first)["preparation_digest"],
                    cache.identity(SOURCE, STARTED, second)["preparation_digest"],
                )

    def test_quarter_boundaries(self):
        cases = [
            ("2026-01-01T00:00:00+00:00", "2026-Q1"),
            ("2026-03-31T23:59:59.999999+00:00", "2026-Q1"),
            ("2026-04-01T00:00:00+00:00", "2026-Q2"),
            ("2026-06-30T23:59:59.999999+00:00", "2026-Q2"),
            ("2026-07-01T00:00:00+00:00", "2026-Q3"),
            ("2026-09-30T23:59:59.999999+00:00", "2026-Q3"),
            ("2026-10-01T00:00:00+00:00", "2026-Q4"),
            ("2026-12-31T23:59:59.999999+00:00", "2026-Q4"),
            ("2027-01-01T00:00:00+00:00", "2027-Q1"),
            ("2028-02-29T12:00:00Z", "2028-Q1"),
            ("2026-05-15T08:30:00.5Z", "2026-Q2"),
        ]
        for started_at, quarter in cases:
            with self.subTest(started_at=started_at):
                result = cache.identity(SOURCE, started_at, INPUTS)
                self.assertEqual(result["quarter"], quarter)
                self.assertIn(f"-{quarter}-", result["cache_key"])

    def test_each_quarter_boundary_changes_the_key(self):
        boundaries = [
            ("2026-03-31T23:59:59.999999+00:00", "2026-04-01T00:00:00+00:00"),
            ("2026-06-30T23:59:59.999999+00:00", "2026-07-01T00:00:00+00:00"),
            ("2026-09-30T23:59:59.999999+00:00", "2026-10-01T00:00:00+00:00"),
            ("2026-12-31T23:59:59.999999+00:00", "2027-01-01T00:00:00+00:00"),
        ]
        for before, after in boundaries:
            with self.subTest(before=before, after=after):
                first = cache.identity(SOURCE, before, INPUTS)
                second = cache.identity(SOURCE, after, INPUTS)
                self.assertEqual(first["preparation_digest"], second["preparation_digest"])
                self.assertNotEqual(first["quarter"], second["quarter"])
                self.assertNotEqual(first["cache_key"], second["cache_key"])

    def test_one_quarter_has_one_key(self):
        instants = (
            "2026-10-01T00:00:00+00:00",
            STARTED,
            "2026-12-31T23:59:59.999999Z",
        )
        keys = {cache.identity(SOURCE, started_at, INPUTS)["cache_key"] for started_at in instants}
        self.assertEqual(len(keys), 1)

    def test_the_same_quarter_of_another_year_has_another_key(self):
        first = cache.identity(SOURCE, "2026-11-06T00:00:00+00:00", INPUTS)
        second = cache.identity(SOURCE, "2027-11-06T00:00:00+00:00", INPUTS)
        self.assertEqual((first["quarter"], second["quarter"]), ("2026-Q4", "2027-Q4"))
        self.assertNotEqual(first["cache_key"], second["cache_key"])

    def test_accepts_what_isoformat_writes(self):
        utc = datetime.timezone.utc
        for instant in (
            datetime.datetime(2026, 3, 31, 23, 59, 59, tzinfo=utc),
            datetime.datetime(2026, 3, 31, 23, 59, 59, 1, tzinfo=utc),
        ):
            with self.subTest(started_at=instant.isoformat()):
                result = cache.identity(SOURCE, instant.isoformat(), INPUTS)
                self.assertEqual(result["quarter"], "2026-Q1")

    def test_a_job_that_crosses_a_quarter_boundary_keeps_its_key(self):
        # The job starts in the last minute of a quarter, and its preparation
        # and verification run in the next quarter. Each step passes the one
        # BENCH_STARTED_AT, so each step computes the same key; the patched
        # clock reads show that the key doesn't come from the clock.
        started = "2026-03-31T23:59:30.000000+00:00"
        after_boundary = datetime.datetime(
            2026, 4, 1, 0, 30, tzinfo=datetime.timezone.utc
        ).timestamp()
        restore = cache.identity(SOURCE, started, INPUTS)
        with mock.patch.object(cache, "datetime", NoClock), mock.patch.object(
            time, "time", return_value=after_boundary
        ), mock.patch.object(time, "time_ns", return_value=int(after_boundary) * 10**9):
            prepare = cache.identity(SOURCE, started, INPUTS)
            verify = cache.identity(SOURCE, started, INPUTS)
        self.assertEqual(restore["quarter"], "2026-Q1")
        self.assertEqual(prepare, restore)
        self.assertEqual(verify, restore)
        later = cache.identity(SOURCE, "2026-04-01T00:30:00+00:00", INPUTS)
        self.assertEqual(later["quarter"], "2026-Q2")
        self.assertNotEqual(later["cache_key"], restore["cache_key"])

    def test_refuses_missing_or_invalid_timestamps(self):
        cases = [
            None,
            1767225600,
            1767225600.0,
            b"2026-11-06T19:47:58+00:00",
            "",
            "2026-11-06",
            "2026-11-06T19:47:58",
            "2026-11-06T19:47:58+01:00",
            "2026-11-06T19:47:58-00:00",
            "2026-11-06 19:47:58+00:00",
            "2026-11-06t19:47:58z",
            "2026-11-06T19:47+00:00",
            "2026-11-06T19:47:58.+00:00",
            "2026-11-06T19:47:58.1234567+00:00",
            " 2026-11-06T19:47:58+00:00",
            "2026-11-06T19:47:58+00:00\n",
            "2026-13-01T00:00:00+00:00",
            "2026-00-01T00:00:00+00:00",
            "2026-02-29T00:00:00+00:00",
            "2026-04-31T00:00:00+00:00",
            "2026-11-06T24:00:00+00:00",
            "2026-11-06T19:60:00+00:00",
            "2026-11-06T19:47:60+00:00",
            "0000-01-01T00:00:00+00:00",
            "\u0662\u0660\u0662\u0666-11-06T19:47:58+00:00",
        ]
        for started_at in cases:
            with self.subTest(started_at=started_at):
                with self.assertRaises(cache.CacheError):
                    cache.identity(SOURCE, started_at, INPUTS)

    def test_refuses_sources_that_arent_nonempty_json_mappings(self):
        cases = [
            None,
            [],
            "https://example.invalid/windows-server-evaluation.iso",
            {},
            {"url": float("nan")},
            {"url": float("inf")},
            {"url": [float("-inf")]},
            {1: "x"},
            {"url": b"x"},
            {"url": ("a",)},
            {"url": {"nested": {2: "x"}}},
            {"url": {"a", "b"}},
            {"url": object()},
        ]
        for source in cases:
            with self.subTest(source=source):
                with self.assertRaises(cache.CacheError):
                    cache.identity(source, STARTED, INPUTS)

    def test_refuses_inputs_that_arent_relative_paths_to_bytes(self):
        cases = [
            None,
            {},
            [("bench/probe/probe.py", b"x")],
            {"/bench/probe/probe.py": b""},
            {"": b""},
            {"bench//probe.py": b""},
            {"./probe.py": b""},
            {"bench/./probe.py": b""},
            {"bench/../probe.py": b""},
            {"..": b""},
            {".": b""},
            {"bench/": b""},
            {"bench\\probe.py": b""},
            {"bench\x00probe.py": b""},
            {"bench/\ud800.py": b""},
            {b"bench/probe.py": b""},
            {"bench/probe.py": b"x", 1: b"y"},
            {"bench/probe.py": "print()"},
            {"bench/probe.py": bytearray(b"x")},
            {"bench/probe.py": None},
        ]
        for inputs in cases:
            with self.subTest(inputs=inputs):
                with self.assertRaises(cache.CacheError):
                    cache.identity(SOURCE, STARTED, inputs)


class MetadataTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = pathlib.Path(directory.name)
        self.image = self.directory / "image.qcow2"
        self.image.write_bytes(IMAGE_BYTES)
        self.identity = cache.identity(SOURCE, STARTED, INPUTS)
        self.metadata = cache.make_metadata(self.identity, self.image, 1234.5, 987654321)

    def test_metadata_schema(self):
        self.assertEqual(cache.METADATA_SCHEMA, "s2n-quic-bench-probe-image")
        self.assertEqual(cache.METADATA_VERSION, 1)
        self.assertEqual(
            self.metadata,
            {
                "schema": "s2n-quic-bench-probe-image",
                "version": 1,
                "identity": self.identity,
                "image": {
                    "sha256": hashlib.sha256(IMAGE_BYTES).hexdigest(),
                    "bytes": len(IMAGE_BYTES),
                },
                "measurements": {
                    "preparation_seconds": 1234.5,
                    "compressed_bytes": 987654321,
                },
            },
        )
        self.assertIsNot(self.metadata["identity"], self.identity)

    def test_seconds_are_stored_as_a_float(self):
        for value, stored in ((12, 12.0), (0, 0.0), (0.25, 0.25)):
            with self.subTest(preparation_seconds=value):
                metadata = cache.make_metadata(self.identity, self.image, value, 1)
                seconds = metadata["measurements"]["preparation_seconds"]
                self.assertIs(type(seconds), float)
                self.assertEqual(seconds, stored)

    def test_metadata_survives_json_and_returns_the_measurements(self):
        parsed = json.loads(json.dumps(self.metadata, allow_nan=False))
        for image_path in (self.image, str(self.image)):
            with self.subTest(image_path=type(image_path).__name__):
                result = cache.validate_metadata(parsed, self.identity, image_path)
                self.assertEqual(
                    result, {"preparation_seconds": 1234.5, "compressed_bytes": 987654321}
                )
                self.assertIs(type(result["preparation_seconds"]), float)
                self.assertIs(type(result["compressed_bytes"]), int)
        self.assertEqual(os.listdir(self.directory), ["image.qcow2"])
        self.assertEqual(self.image.read_bytes(), IMAGE_BYTES)

    def test_metadata_from_before_a_quarter_boundary_validates_after_it(self):
        started = "2026-03-31T23:59:30+00:00"
        prepared = cache.identity(SOURCE, started, INPUTS)
        metadata = cache.make_metadata(prepared, self.image, 3600.0, 1000)
        with mock.patch.object(cache, "datetime", NoClock):
            verified = cache.identity(SOURCE, started, INPUTS)
            result = cache.validate_metadata(metadata, verified, self.image)
        self.assertEqual(result, {"preparation_seconds": 3600.0, "compressed_bytes": 1000})
        next_quarter = cache.identity(SOURCE, "2026-04-01T00:00:00+00:00", INPUTS)
        with self.assertRaisesRegex(cache.CacheError, "metadata quarter 2026-Q1 "):
            cache.validate_metadata(metadata, next_quarter, self.image)

    def test_image_is_hashed_in_chunks(self):
        reads = []
        real_read = os.read

        def recording_read(descriptor, size):
            reads.append(size)
            return real_read(descriptor, size)

        with mock.patch.object(cache, "_CHUNK_BYTES", 7), mock.patch.object(
            os, "read", recording_read
        ):
            metadata = cache.make_metadata(self.identity, self.image, 1.0, 1)
            cache.validate_metadata(metadata, self.identity, self.image)
        self.assertEqual(metadata["image"]["sha256"], hashlib.sha256(IMAGE_BYTES).hexdigest())
        self.assertEqual(set(reads), {7})
        self.assertGreater(len(reads), 2 * (len(IMAGE_BYTES) // 7))

    def test_make_metadata_refuses_invalid_measurements(self):
        seconds_cases = [
            True,
            False,
            None,
            "12",
            [12],
            float("nan"),
            float("inf"),
            float("-inf"),
            -1,
            -0.5,
            10**400,
        ]
        for value in seconds_cases:
            with self.subTest(preparation_seconds=value):
                with self.assertRaises(cache.CacheError):
                    cache.make_metadata(self.identity, self.image, value, 1)
        bytes_cases = [True, False, None, 0, -1, 1.0, 2.5, "100", [1]]
        for value in bytes_cases:
            with self.subTest(compressed_bytes=value):
                with self.assertRaises(cache.CacheError):
                    cache.make_metadata(self.identity, self.image, 1.0, value)

    def broken_identities(self):
        def changed(**fields):
            result = copy.deepcopy(self.identity)
            result.update(fields)
            return result

        missing = copy.deepcopy(self.identity)
        del missing["quarter"]
        return [
            ("not a mapping", ["source"]),
            ("missing field", missing),
            ("extra field", changed(started_at=STARTED)),
            ("empty source", changed(source={})),
            ("source not a mapping", changed(source="url")),
            ("source changed, key kept", changed(source=dict(SOURCE, edition="other"))),
            ("uppercase digest", changed(preparation_digest="A" * 64)),
            ("short digest", changed(preparation_digest="a" * 63)),
            ("digest not a string", changed(preparation_digest=None)),
            ("quarter out of range", changed(quarter="2026-Q5")),
            ("quarter without a dash", changed(quarter="2026Q4")),
            ("quarter not a string", changed(quarter=4)),
            ("quarter changed, key kept", changed(quarter="2026-Q3")),
            ("tampered key", changed(cache_key=self.identity["cache_key"] + "0")),
            ("key not a string", changed(cache_key=None)),
        ]

    def test_make_metadata_checks_the_identity(self):
        for label, identity in self.broken_identities():
            with self.subTest(identity=label):
                with self.assertRaises(cache.CacheError):
                    cache.make_metadata(identity, self.image, 1.0, 1)

    def test_validate_metadata_checks_the_expected_identity(self):
        for label, identity in self.broken_identities():
            with self.subTest(identity=label):
                with self.assertRaises(cache.CacheError):
                    cache.validate_metadata(self.metadata, identity, self.image)
        tampered = copy.deepcopy(self.identity)
        tampered["cache_key"] += "0"
        with self.assertRaisesRegex(cache.CacheError, "expected_identity cache_key"):
            cache.validate_metadata(self.metadata, tampered, self.image)

    def test_refuses_image_paths_that_arent_regular_files(self):
        cases = [
            ("absent", self.directory / "missing.qcow2"),
            ("directory", self.directory),
        ]
        if hasattr(os, "symlink"):
            link = self.directory / "link.qcow2"
            os.symlink(self.image, link)
            cases.append(("symbolic link", link))
            dangling = self.directory / "dangling.qcow2"
            os.symlink(self.directory / "missing.qcow2", dangling)
            cases.append(("dangling symbolic link", dangling))
        if hasattr(os, "mkfifo"):
            fifo = self.directory / "fifo.qcow2"
            os.mkfifo(fifo)
            cases.append(("named pipe", fifo))
        for label, path in cases:
            with self.subTest(image=label):
                with self.assertRaises(cache.CacheError):
                    cache.make_metadata(self.identity, path, 1.0, 1)
                with self.assertRaises(cache.CacheError):
                    cache.validate_metadata(self.metadata, self.identity, path)
        if hasattr(os, "symlink"):
            with self.assertRaisesRegex(cache.CacheError, "symbolic link"):
                cache.validate_metadata(self.metadata, self.identity, link)

    def test_refuses_image_paths_of_other_types(self):
        for value in (None, 42, ["image.qcow2"], "image\x00.qcow2"):
            with self.subTest(image_path=value):
                with self.assertRaises(cache.CacheError):
                    cache.make_metadata(self.identity, value, 1.0, 1)
                with self.assertRaises(cache.CacheError):
                    cache.validate_metadata(self.metadata, self.identity, value)

    def test_refuses_an_empty_image(self):
        empty = self.directory / "empty.qcow2"
        empty.write_bytes(b"")
        with self.assertRaisesRegex(cache.CacheError, "is empty"):
            cache.make_metadata(self.identity, empty, 1.0, 1)

    def malformed(self):
        def changed(path, value):
            result = copy.deepcopy(self.metadata)
            target = result
            for name in path[:-1]:
                target = target[name]
            target[path[-1]] = value
            return result

        def removed(path):
            result = copy.deepcopy(self.metadata)
            target = result
            for name in path[:-1]:
                target = target[name]
            del target[path[-1]]
            return result

        cases = [
            ("None", None),
            ("list", []),
            ("JSON text", json.dumps(self.metadata)),
        ]
        for name in ("schema", "version", "identity", "image", "measurements"):
            cases.append((f"missing {name}", removed((name,))))
        for name in ("source", "preparation_digest", "quarter", "cache_key"):
            cases.append((f"missing identity {name}", removed(("identity", name))))
        for name in ("sha256", "bytes"):
            cases.append((f"missing image {name}", removed(("image", name))))
        for name in ("preparation_seconds", "compressed_bytes"):
            cases.append((f"missing {name}", removed(("measurements", name))))
        digest = hashlib.sha256(IMAGE_BYTES).hexdigest()
        cases += [
            ("extra field", changed(("output",), "install log")),
            ("extra identity field", changed(("identity", "started_at"), STARTED)),
            ("extra image field", changed(("image", "path"), "image.qcow2")),
            ("extra measurement", changed(("measurements", "password"), "x")),
            ("other schema", changed(("schema",), "other")),
            ("schema not a string", changed(("schema",), None)),
            ("version as text", changed(("version",), "1")),
            ("version as float", changed(("version",), 1.0)),
            ("version as bool", changed(("version",), True)),
            ("newer version", changed(("version",), 2)),
            ("older version", changed(("version",), 0)),
            ("identity not a mapping", changed(("identity",), "key")),
            ("empty source", changed(("identity", "source"), {})),
            ("uppercase digest", changed(("identity", "preparation_digest"), "A" * 64)),
            ("quarter out of range", changed(("identity", "quarter"), "2026-Q0")),
            (
                "tampered key",
                changed(("identity", "cache_key"), self.metadata["identity"]["cache_key"] + "0"),
            ),
            ("image not a mapping", changed(("image",), digest)),
            ("uppercase sha256", changed(("image", "sha256"), "A" * 64)),
            ("short sha256", changed(("image", "sha256"), digest[:-1])),
            ("sha256 not a string", changed(("image", "sha256"), 0)),
            ("image bytes as bool", changed(("image", "bytes"), True)),
            ("image bytes zero", changed(("image", "bytes"), 0)),
            ("image bytes as text", changed(("image", "bytes"), str(len(IMAGE_BYTES)))),
            ("image bytes as float", changed(("image", "bytes"), float(len(IMAGE_BYTES)))),
            ("measurements not a mapping", changed(("measurements",), [1234.5, 987654321])),
            ("seconds NaN", changed(("measurements", "preparation_seconds"), float("nan"))),
            ("seconds infinite", changed(("measurements", "preparation_seconds"), float("inf"))),
            ("seconds negative", changed(("measurements", "preparation_seconds"), -1.0)),
            ("seconds as bool", changed(("measurements", "preparation_seconds"), True)),
            ("seconds as text", changed(("measurements", "preparation_seconds"), "1234.5")),
            ("seconds null", changed(("measurements", "preparation_seconds"), None)),
            ("compressed bytes zero", changed(("measurements", "compressed_bytes"), 0)),
            ("compressed bytes as bool", changed(("measurements", "compressed_bytes"), True)),
            (
                "compressed bytes as float",
                changed(("measurements", "compressed_bytes"), 987654321.0),
            ),
            (
                "compressed bytes as text",
                changed(("measurements", "compressed_bytes"), "987654321"),
            ),
        ]
        return cases

    def test_refuses_malformed_metadata(self):
        for label, metadata in self.malformed():
            with self.subTest(metadata=label):
                with self.assertRaises(cache.CacheError):
                    cache.validate_metadata(metadata, self.identity, self.image)

    def test_refuses_a_malformed_image_hash(self):
        metadata = copy.deepcopy(self.metadata)
        metadata["image"]["sha256"] = "A" * 64
        with self.assertRaisesRegex(cache.CacheError, "64 lowercase hex digits"):
            cache.validate_metadata(metadata, self.identity, self.image)

    def test_refuses_an_unsupported_version(self):
        metadata = copy.deepcopy(self.metadata)
        metadata["version"] = 2
        with self.assertRaisesRegex(cache.CacheError, "version 2 isn't supported"):
            cache.validate_metadata(metadata, self.identity, self.image)

    def test_refuses_nonfinite_numbers_parsed_from_json(self):
        text = json.dumps(self.metadata)
        self.assertEqual(text.count("1234.5"), 1)
        for constant in ("NaN", "Infinity", "-Infinity"):
            with self.subTest(preparation_seconds=constant):
                metadata = json.loads(text.replace("1234.5", constant))
                with self.assertRaises(cache.CacheError):
                    cache.validate_metadata(metadata, self.identity, self.image)

    def test_refuses_stale_metadata_without_changing_it(self):
        cases = [
            ("quarter", cache.identity(SOURCE, "2027-01-01T00:00:00+00:00", INPUTS)),
            (
                "preparation_digest",
                cache.identity(SOURCE, STARTED, with_input(SCRIPT, INPUTS[SCRIPT] + b"#\n")),
            ),
            (
                "source",
                cache.identity(
                    dict(SOURCE, edition="Windows Server Standard Evaluation"), STARTED, INPUTS
                ),
            ),
        ]
        for field, expected in cases:
            with self.subTest(field=field):
                metadata = copy.deepcopy(self.metadata)
                with self.assertRaisesRegex(cache.CacheError, f"metadata {field} "):
                    cache.validate_metadata(metadata, expected, self.image)
                self.assertEqual(metadata, self.metadata)
        self.assertEqual(self.image.read_bytes(), IMAGE_BYTES)
        self.assertEqual(os.listdir(self.directory), ["image.qcow2"])

    def test_refuses_a_changed_image(self):
        flipped = bytearray(IMAGE_BYTES)
        flipped[100] ^= 1
        cases = [
            ("one byte changed", bytes(flipped), "SHA-256"),
            ("replaced", b"x" * len(IMAGE_BYTES), "SHA-256"),
            ("byte appended", IMAGE_BYTES + b"\x00", "is 4101 bytes"),
            ("truncated", IMAGE_BYTES[:-1], "is 4099 bytes"),
        ]
        for label, content, message in cases:
            with self.subTest(image=label):
                self.image.write_bytes(content)
                with self.assertRaisesRegex(cache.CacheError, message):
                    cache.validate_metadata(self.metadata, self.identity, self.image)


if __name__ == "__main__":
    unittest.main()
