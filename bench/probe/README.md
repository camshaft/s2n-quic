# Windows evaluation image probe

This helper prepares and checks one disposable Windows Server evaluation guest.
It does not publish or start a workflow, and deterministic tests are not evidence
of a Windows boot. See [windows/README.md](windows/README.md) for the public media,
unattended setup, SSH trust, evaluation conditions and required live checks.

## Workflow interface

The workflow captures `BENCH_STARTED_AT` once, before checkout, as a UTC ISO 8601
timestamp ending in `Z` or `+00:00`, and preserves it through these commands:

```text
python3 bench/probe/probe.py kvm --result-dir DIR
python3 bench/probe/probe.py identity --github-output FILE
python3 bench/probe/probe.py prepare --image-dir DIR --result-dir RESULTS
python3 bench/probe/probe.py verify --image-dir DIR --result-dir RESULTS
```

`kvm` checks existence, write access and an actual read/write open of `/dev/kvm`.
Failure prints `bench: no-kvm`, appends a result and exits nonzero. `prepare` and
`verify` repeat this check before reading identity inputs or starting the backend.
`identity` appends one `cache_key=VALUE` line to the workflow output file. Its failure
writes `bench: identity-failed` to stderr, exits nonzero and does not append a key.

The job supplies `BENCH_MEMORY_MIB`, `BENCH_DISK_GIB`, `BENCH_TIMEOUT_SECONDS` and
`BENCH_CLEANUP_SECONDS` as positive integer strings before `identity`. The backend
checks available memory and disk against the requested guest size. Deadlines are
job inputs, not a promise of installation speed. The helper runs on the main thread
because the backend handles SIGINT and SIGTERM. The host needs Python 3.11 or later
and these Ubuntu packages:

```text
qemu-system-x86 qemu-utils genisoimage p7zip-full curl wimtools openssh-client
```

`zstd` is not used. `compressed_bytes` measures the zlib-compressed qcow2 file from
`qemu-img convert -c`, not the Actions cache archive. No installation-size or timing
measurement is claimed until the real preparation runs.

## Identity and cache

Use an exact cache key, without restore prefixes. The key includes the image source,
the UTC calendar quarter of `BENCH_STARTED_AT`, and a digest of paths and bytes from
`probe.py`, `cache.py`, `guest.py`, the backend's Windows preparation inputs, and a
canonical JSON representation of its four runtime settings. Tests and generated
results are excluded. A quarter transition during a job does not change its key.
Changing source, preparation inputs, settings or quarter invalidates the key.

Cache exactly `image.qcow2` and `prepared.json`. The latter is an envelope with
`version: 1`, `provenance` (the cache module's unchanged version-1 metadata), and
`preparation` (the original workflow start, compression description, measured ISO
source evidence and sanitized preparation attestations). Preparation uses staging
files and publishes the completed pair without replacing existing cache files.
A failure between publication of the two files removes the newly published image.
An interrupted process or cache extraction can leave a partial entry; verification
rejects it instead of repairing or refreshing it. Save the cache only after the
verify command succeeds. Do not upload the image or prepared.json as probe logs.

The result and image directories must be disjoint. Private work is created with
mode 0700 under `RUNNER_TEMP`, or the platform temporary directory when it is unset,
and must be outside both directories. The backend's per-run keys, seed and overlay
stay there; cleanup completes before verification succeeds. Upload only the result
directory's JSON lines. Raw command output and private work are not result artifacts.

## Results and failures

Results append to `probe.jsonl`; existing workflow result files remain unchanged.
A failed result write exits nonzero with `bench: result-failed`, rather than printing
success without a record. Result and metadata files are not followed through symlinks.

Preparation prints `bench: prepared`, never `bench: ok`. Verification checks the
metadata schema, types, identity, image size and SHA-256 before starting a guest.
It rejects malformed JSON, duplicate fields, stale keys, changed images and invalid
evidence. Only after authenticated SSH, the expected command, clean guest shutdown,
QEMU exit and cleanup does it append an `ok` result and print exactly `bench: ok`.
Preparation and verification failures have distinct statuses and closed stage/code
fields; they do not include exception messages, subprocess output or credentials.
Cleanup error codes remain separate from the primary failure.

The final successful JSON line includes `compressed_bytes`, `preparation_seconds`,
the current `started_at`, `cache_key`, the cached `preparation` evidence, and the
current boot's `guest_evidence`, including `license_status` and `grace_minutes`.
The preparation start is not an installation timestamp: `installation_time` is null
and `installation_time_status` is `unavailable` because the backend does not export
that timestamp. Failures before Windows retain null guest evidence. The publisher
hash is null when the public descriptor lacks one; the downloaded ISO's SHA-256 is
explicitly measured, not a publisher attestation. A quarterly key does not prove
that an offline evaluation remains usable for a quarter. Licensing checks remain in
the guest and do not activate, reset or extend the evaluation.

## Deterministic tests

```text
python3 -m unittest discover -s bench/probe/tests
```

The suite uses Python's standard library. `test_cache.py` checks identity and image
metadata, `test_guest.py` simulates process/guest failures and checks configuration,
and `test_probe.py` exercises the CLI with temporary files, injected failures and a
combined path through both modules with simulated processes. These tests do not
perform a real download, Windows install, guest boot or SSH session.
