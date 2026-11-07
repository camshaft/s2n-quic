# Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

"""Cache identity and provenance of the bench probe's prepared Windows image.

The bench probe keeps one prepared Windows Server evaluation image in the
GitHub Actions cache, as two files: image.qcow2 and prepared.json. This module
computes the exact cache key of that image, builds the metadata kept in
prepared.json, and checks that metadata and the image before a run uses them.
It uses the Python standard library alone. It doesn't read the clock, and it
reads only the image file it is passed. It doesn't write any file: the caller
parses and publishes prepared.json.

Each function raises CacheError, a subclass of ValueError, for an argument it
refuses, with a message that says which value is wrong.

identity(source, started_at, inputs) -> dict

    source
        A nonempty mapping of JSON values that describes the exact public
        image source and edition, such as the ISO's public URL, its published
        SHA-256 and the edition's name. Keys are strings; values are str, int,
        finite float, bool, None, list, or a mapping of the same. Keep
        credentials out of it: it is stored in prepared.json.
    started_at
        The workflow's BENCH_STARTED_AT: a UTC timestamp in the form that
        datetime.isoformat() writes for an aware UTC datetime, such as
        2026-11-06T19:47:58.336686+00:00. "Z" is accepted in place of
        "+00:00", and the fraction of a second is optional (1 to 6 digits).
        The workflow records it once, before checkout, and passes the same
        value to every step, so the quarter doesn't change during a long
        preparation.
    inputs
        A nonempty mapping from the path of each preparation input, relative
        to the repository's root in POSIX form, such as bench/probe/probe.py,
        to that file's bytes. Pass every file the preparation reads, such as
        the helper's scripts and the setup configuration, and not tests or
        result files.

    Returns a new dict with exactly these keys, in this order:

        source              a copy of source, made of plain dicts and lists
        preparation_digest  64 lowercase hex digits, the SHA-256 of the inputs
        quarter             the UTC calendar quarter of started_at, such as
                            2026-Q4
        cache_key           bench-probe-windows-image-v1-<quarter>-<64 hex>

    The preparation digest hashes a fixed prefix, the count of inputs and
    then, for each input in the order of its path's UTF-8 bytes, the path's
    length, the path, the content's length and the content, each length as
    8 big-endian bytes. A changed, added or removed path, or a changed byte
    of any input, changes the digest. The hex part of the cache key is the
    SHA-256 of the canonical JSON (sorted keys, no spaces, ASCII) of the
    source, the preparation digest and the quarter, so a change of any of the
    three changes the key, whatever the order of the mappings passed in.

make_metadata(identity, image_path, preparation_seconds, compressed_bytes) -> dict

    Checks identity, a dict that identity() returned, hashes the image at
    image_path in chunks, and returns the metadata to store as prepared.json:

        {
            "schema": "s2n-quic-bench-probe-image",
            "version": 1,
            "identity": {
                "source": ...,
                "preparation_digest": ...,
                "quarter": ...,
                "cache_key": ...
            },
            "image": {"sha256": "<64 lowercase hex>", "bytes": <int >= 1>},
            "measurements": {
                "preparation_seconds": <float >= 0>,
                "compressed_bytes": <int >= 1>
            }
        }

    preparation_seconds is an int or a float that is finite and not negative,
    stored as a float; compressed_bytes is an int of at least 1; a bool is
    refused for either. The image must be a nonempty regular file and not a
    symbolic link. The metadata holds only these fields, so apart from the
    source the caller passes it doesn't hold credentials or another
    program's output.

validate_metadata(metadata, expected_identity, image_path) -> dict

    Checks metadata, such as prepared.json as the caller parsed it, against
    expected_identity, this run's identity(), and against the image at
    image_path. It refuses metadata with a missing or an unexpected field, a
    value of the wrong type, another schema or an unsupported version, a
    source, preparation digest, quarter or cache key that differs from this
    run's, or a malformed hash, and an image that is absent, a symbolic link,
    not a regular file, or of another size or SHA-256 than the metadata
    records. Only after every check passes does it return the measurements:

        {"preparation_seconds": <float>, "compressed_bytes": <int>}

    It doesn't change metadata or the image. Stale metadata is an error for
    the caller to act on: this module never refreshes it.
"""

import hashlib
import json
import math
import os
import re
import stat
from collections.abc import Mapping
from datetime import datetime, timezone

__all__ = [
    "CACHE_KEY_PREFIX",
    "METADATA_SCHEMA",
    "METADATA_VERSION",
    "CacheError",
    "identity",
    "make_metadata",
    "validate_metadata",
]

CACHE_KEY_PREFIX = "bench-probe-windows-image-v1"
METADATA_SCHEMA = "s2n-quic-bench-probe-image"
METADATA_VERSION = 1

# The size of each read while hashing the image, so that the image is never
# held in memory whole.
_CHUNK_BYTES = 1 << 20

_INPUTS_PREFIX = b"s2n-quic bench probe preparation inputs v1\n"

_IDENTITY_FIELDS = ("source", "preparation_digest", "quarter", "cache_key")
_IMAGE_FIELDS = ("sha256", "bytes")
_MEASUREMENT_FIELDS = ("preparation_seconds", "compressed_bytes")
_METADATA_FIELDS = ("schema", "version", "identity", "image", "measurements")

_STARTED_AT = re.compile(
    r"([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})"
    r"(?:\.([0-9]{1,6}))?(?:Z|\+00:00)"
)
_SHA256_HEX = re.compile(r"[0-9a-f]{64}")
_QUARTER = re.compile(r"[0-9]{4}-Q[1-4]")


class CacheError(ValueError):
    """An argument that the cache identity or provenance checks refuse."""


def identity(source, started_at, inputs):
    """Return the cache identity of the image, as the module's documentation says."""
    checked_source = _check_source(source, "source")
    quarter = _quarter(started_at)
    preparation_digest = _preparation_digest(inputs)
    return {
        "source": checked_source,
        "preparation_digest": preparation_digest,
        "quarter": quarter,
        "cache_key": _cache_key(checked_source, preparation_digest, quarter),
    }


def make_metadata(identity, image_path, preparation_seconds, compressed_bytes):
    """Return the metadata to store beside the image, as the module's documentation says."""
    checked = _check_identity(identity, "identity")
    seconds = _seconds(preparation_seconds, "preparation_seconds")
    compressed = _byte_count(compressed_bytes, "compressed_bytes")
    image = _hash_image(image_path, None)
    if image["bytes"] < 1:
        raise CacheError(f"image {os.fspath(image_path)!r} is empty")
    return {
        "schema": METADATA_SCHEMA,
        "version": METADATA_VERSION,
        "identity": checked,
        "image": image,
        "measurements": {
            "preparation_seconds": seconds,
            "compressed_bytes": compressed,
        },
    }


def validate_metadata(metadata, expected_identity, image_path):
    """Check metadata and the image, then return the measurements, as the module's documentation says."""
    expected = _check_identity(expected_identity, "expected_identity")
    fields = _fields(metadata, _METADATA_FIELDS, "metadata")

    schema = fields["schema"]
    if not isinstance(schema, str) or schema != METADATA_SCHEMA:
        raise CacheError(f"metadata schema is {schema!r}, not {METADATA_SCHEMA!r}")
    version = fields["version"]
    if isinstance(version, bool) or not isinstance(version, int):
        raise CacheError(f"metadata version must be an integer, not {_kind(version)}")
    if version != METADATA_VERSION:
        raise CacheError(
            f"metadata version {version} isn't supported: this helper reads"
            f" version {METADATA_VERSION}"
        )

    stored = _check_identity(fields["identity"], "metadata identity")
    for name in _IDENTITY_FIELDS:
        if _canonical(stored[name]) != _canonical(expected[name]):
            if name == "source":
                raise CacheError("metadata source doesn't match this run's image source")
            raise CacheError(
                f"metadata {name} {stored[name]} doesn't match this run's {expected[name]}"
            )

    image = _fields(fields["image"], _IMAGE_FIELDS, "metadata image")
    sha256 = image["sha256"]
    if not isinstance(sha256, str) or _SHA256_HEX.fullmatch(sha256) is None:
        raise CacheError("metadata image sha256 isn't 64 lowercase hex digits")
    size = _byte_count(image["bytes"], "metadata image bytes")

    measurements = _fields(fields["measurements"], _MEASUREMENT_FIELDS, "metadata measurements")
    seconds = _seconds(measurements["preparation_seconds"], "metadata preparation_seconds")
    compressed = _byte_count(measurements["compressed_bytes"], "metadata compressed_bytes")

    actual = _hash_image(image_path, size)
    if actual["sha256"] != sha256:
        raise CacheError(
            f"image SHA-256 {actual['sha256']} doesn't match the metadata's {sha256}"
        )
    return {"preparation_seconds": seconds, "compressed_bytes": compressed}


def _kind(value):
    return type(value).__name__


def _canonical(value):
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=True, allow_nan=False
    ).encode("ascii")


def _json_value(value, where):
    """Return a copy of value made of plain JSON types, or raise CacheError."""
    if value is None or isinstance(value, (bool, str, int)):
        return value
    if isinstance(value, float):
        if not math.isfinite(value):
            raise CacheError(f"{where} is {value!r}, which JSON can't represent")
        return value
    if isinstance(value, Mapping):
        copied = {}
        for key, item in value.items():
            if not isinstance(key, str):
                raise CacheError(f"{where} has the key {key!r}, which isn't a string")
            copied[key] = _json_value(item, f"{where}[{key!r}]")
        return copied
    if isinstance(value, list):
        return [_json_value(item, f"{where}[{index}]") for index, item in enumerate(value)]
    raise CacheError(f"{where} is a {_kind(value)}, which isn't a JSON value")


def _check_source(source, where):
    if not isinstance(source, Mapping):
        raise CacheError(f"{where} must be a mapping, not {_kind(source)}")
    if not source:
        raise CacheError(f"{where} must not be empty")
    return _json_value(source, where)


def _quarter(started_at):
    if not isinstance(started_at, str):
        raise CacheError(f"started_at must be a string, not {_kind(started_at)}")
    match = _STARTED_AT.fullmatch(started_at)
    if match is None:
        raise CacheError(
            f"started_at {started_at!r} isn't a UTC timestamp such as"
            " 2026-01-01T00:00:00+00:00"
        )
    year, month, day, hour, minute, second, fraction = match.groups()
    try:
        datetime(
            int(year),
            int(month),
            int(day),
            int(hour),
            int(minute),
            int(second),
            int((fraction or "").ljust(6, "0")),
            tzinfo=timezone.utc,
        )
    except ValueError as error:
        raise CacheError(f"started_at {started_at!r} isn't a valid time: {error}") from None
    return f"{year}-Q{(int(month) - 1) // 3 + 1}"


def _input_path(path):
    """Return the UTF-8 bytes of a normalized relative POSIX path, or raise CacheError."""
    if not isinstance(path, str):
        raise CacheError(f"input path {path!r} must be a string, not {_kind(path)}")
    if not path or path.startswith("/") or "\\" in path or "\x00" in path:
        raise CacheError(f"input path {path!r} isn't a relative POSIX path")
    if any(part in ("", ".", "..") for part in path.split("/")):
        raise CacheError(f"input path {path!r} isn't a normalized relative path")
    try:
        return path.encode("utf-8")
    except UnicodeEncodeError:
        raise CacheError(f"input path {path!r} isn't valid Unicode text") from None


def _preparation_digest(inputs):
    if not isinstance(inputs, Mapping):
        raise CacheError(f"inputs must be a mapping of paths to bytes, not {_kind(inputs)}")
    if not inputs:
        raise CacheError("inputs must hold at least one preparation input")
    entries = []
    for path, content in inputs.items():
        encoded = _input_path(path)
        if not isinstance(content, bytes):
            raise CacheError(f"input {path!r} must be bytes, not {_kind(content)}")
        entries.append((encoded, content))
    entries.sort(key=lambda entry: entry[0])
    digest = hashlib.sha256(_INPUTS_PREFIX)
    digest.update(len(entries).to_bytes(8, "big"))
    for encoded, content in entries:
        digest.update(len(encoded).to_bytes(8, "big"))
        digest.update(encoded)
        digest.update(len(content).to_bytes(8, "big"))
        digest.update(content)
    return digest.hexdigest()


def _cache_key(source, preparation_digest, quarter):
    material = _canonical(
        {"preparation_digest": preparation_digest, "quarter": quarter, "source": source}
    )
    return f"{CACHE_KEY_PREFIX}-{quarter}-{hashlib.sha256(material).hexdigest()}"


def _fields(value, names, where):
    """Return value's fields named in names, or raise CacheError where value holds others."""
    if not isinstance(value, Mapping):
        raise CacheError(f"{where} must be an object, not {_kind(value)}")
    missing = [name for name in names if name not in value]
    if missing:
        raise CacheError(f"{where} is missing {', '.join(missing)}")
    unexpected = sorted(repr(key) for key in value if key not in names)
    if unexpected:
        raise CacheError(f"{where} has unexpected fields: {', '.join(unexpected)}")
    return {name: value[name] for name in names}


def _check_identity(value, where):
    fields = _fields(value, _IDENTITY_FIELDS, where)
    source = _check_source(fields["source"], f"{where} source")
    digest = fields["preparation_digest"]
    if not isinstance(digest, str) or _SHA256_HEX.fullmatch(digest) is None:
        raise CacheError(f"{where} preparation_digest isn't 64 lowercase hex digits")
    quarter = fields["quarter"]
    if not isinstance(quarter, str) or _QUARTER.fullmatch(quarter) is None:
        raise CacheError(f"{where} quarter {quarter!r} isn't a quarter such as 2026-Q1")
    key = fields["cache_key"]
    if not isinstance(key, str):
        raise CacheError(f"{where} cache_key must be a string, not {_kind(key)}")
    if key != _cache_key(source, digest, quarter):
        raise CacheError(
            f"{where} cache_key doesn't match its source, preparation_digest and quarter"
        )
    return {
        "source": source,
        "preparation_digest": digest,
        "quarter": quarter,
        "cache_key": key,
    }


def _seconds(value, where):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise CacheError(f"{where} must be a number of seconds, not {_kind(value)}")
    try:
        seconds = float(value)
    except OverflowError:
        raise CacheError(f"{where} is too large to store as seconds") from None
    if not math.isfinite(seconds) or seconds < 0:
        raise CacheError(f"{where} must be finite and not negative, not {value!r}")
    return seconds


def _byte_count(value, where):
    if isinstance(value, bool) or not isinstance(value, int):
        raise CacheError(f"{where} must be a whole number of bytes, not {_kind(value)}")
    if value < 1:
        raise CacheError(f"{where} must be at least 1, not {value}")
    return int(value)


def _hash_image(image_path, expected_bytes):
    """Return the image's SHA-256 and size, read in chunks, or raise CacheError.

    Where expected_bytes isn't None, a file of another size is refused before
    it is read.
    """
    try:
        path = os.fspath(image_path)
    except TypeError:
        raise CacheError(f"image_path must be a path, not {_kind(image_path)}") from None
    try:
        before = os.lstat(path)
    except FileNotFoundError:
        raise CacheError(f"image {path!r} doesn't exist") from None
    except (OSError, ValueError) as error:
        raise CacheError(f"image {path!r} can't be read: {error}") from None
    if stat.S_ISLNK(before.st_mode):
        raise CacheError(f"image {path!r} is a symbolic link")
    if not stat.S_ISREG(before.st_mode):
        raise CacheError(f"image {path!r} isn't a regular file")
    # O_NOFOLLOW refuses a symbolic link put in the image's place after the
    # check above, and O_NONBLOCK keeps a named pipe put there from blocking
    # the open; the fstat check below then refuses either.
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise CacheError(f"image {path!r} can't be opened: {error}") from None
    try:
        opened = os.fstat(descriptor)
        if not stat.S_ISREG(opened.st_mode) or (opened.st_dev, opened.st_ino) != (
            before.st_dev,
            before.st_ino,
        ):
            raise CacheError(f"image {path!r} changed while it was opened")
        if expected_bytes is not None and opened.st_size != expected_bytes:
            raise CacheError(
                f"image {path!r} is {opened.st_size} bytes, but the metadata records"
                f" {expected_bytes}"
            )
        digest = hashlib.sha256()
        size = 0
        while True:
            chunk = os.read(descriptor, _CHUNK_BYTES)
            if not chunk:
                break
            size += len(chunk)
            digest.update(chunk)
    except OSError as error:
        raise CacheError(f"image {path!r} can't be read: {error}") from None
    finally:
        os.close(descriptor)
    if expected_bytes is not None and size != expected_bytes:
        raise CacheError(
            f"image {path!r} is {size} bytes, but the metadata records {expected_bytes}"
        )
    return {"sha256": digest.hexdigest(), "bytes": size}
