# Windows evaluation probe backend

This directory supports `guest.py`; it does not start a workflow. Its current
validation is deterministic host testing and configuration inspection. A live
Windows install, boot, and SSH session must be checked separately before using
the image for benchmarks.

## API and host requirements

- `image_source()` returns the static JSON descriptor in `source.json`.
- `preparation_inputs()` returns repository-relative paths. Hash their bytes,
  together with the integrating CLI and cache module, before restoring a cache.
- `prepare_image(image_path, work_dir)` creates a previously absent image and
  returns `preparation_seconds`, `compressed_bytes`, `compression`,
  `source_evidence`, and `guest_evidence`. Preserve the last three fields in the
  run's result record; the image also contains its source evidence. Returned
  guest evidence contains only declared fields, excluding extra guest text and
  the per-run protocol nonce.
- `verify_image(image_path, work_dir)` returns `None` only after authenticated SSH,
  the guest's shutdown event, QEMU exit, and temporary-file cleanup.
- Failures raise `GuestError` with `stage` and `code`; these fields contain neither
  subprocess output nor credentials. If cleanup also fails, `cleanup` preserves
  its closed error codes beside the original stage and code.

The caller first validates cache metadata, including the image digest. This
backend does not replace provenance validation and never prints `bench: ok`.

Use x86-64 Linux with readable/writable `/dev/kvm`, Linux pidfds, Python 3.9 or
later, and Ubuntu packages `qemu-system-x86`, `qemu-utils`, `genisoimage`,
`p7zip-full`, `wimtools`, `curl`, and `openssh-client`. This backend does not need
`zstd`: compression is `qemu-img convert -c -O qcow2 -o compression_type=zlib`.
`compressed_bytes` is the resulting qcow2 file length, not an estimate of a later
cache archive. The preparation timer includes download through the final copy.

Set `BENCH_MEMORY_MIB`, `BENCH_DISK_GIB`, `BENCH_TIMEOUT_SECONDS`, and
`BENCH_CLEANUP_SECONDS` to positive integers appropriate to the runner and its job
deadline. There are no implicit resource or duration defaults. The disk check
requires free space for two virtual-disk lengths for installation plus conversion;
it checks that space again after downloading and inspecting the media. Memory and free disk are read before starting the guest.
Choose a job deadline that allows the operation and its cleanup allowance.
The integrating caller must run these synchronous APIs on the main thread so
SIGINT and SIGTERM can be recorded for cleanup. They end the operation as
`GuestError` stage `interrupted`, code `signal`, at the next safe checkpoint;
a running dependency may finish or reach its configured timeout first.
Signals never raise during resource acquisition or cleanup.

`work_dir` must be owned by the caller, mode 0700, and outside both the image
cache and uploaded result directories. A new work directory is created with
mode 0700; an existing less restrictive one is refused. Each operation uses a
private temporary subdirectory. Do not upload that directory, a disk overlay,
seed media, or raw subprocess output. Cache only `image.qcow2` and the caller's
`prepared.json`. The caller publishes metadata only after preparation returns.

## Image source and evidence

The source is Microsoft's public English Windows Server 2025 evaluation ISO,
linked from <https://www.microsoft.com/en-us/evalcenter/download-windows-server-2025>.
The descriptor selects Standard Evaluation with Server Core. The installer reads
the WIM's XML, selects a unique matching display name, installation type and x64
architecture, and carries its actual image index and EditionID into setup. It
refuses a changed or ambiguous selection rather than guessing an index.

The public page does not supply a publisher SHA-256. `publisher_sha256: null`
records that absence. HTTPS certificate validation and HTTPS-only redirects are
required. The final URL must be on the descriptor's download-host list and have
neither credentials nor a query. Preparation records that final URL, byte length,
and a computed SHA-256 explicitly marked `measured`. This is integrity evidence
for the bytes downloaded, not a publisher signature or a pinned publisher hash.
A publisher digest can be configured when authoritative evidence is available;
that changes the source identity and then must match the downloaded bytes.

The public page states both a 180-day evaluation and a requirement to activate
over the internet in the first 10 days to avoid automatic shutdown. Preparation
has no network adapter. Verification uses QEMU's restricted user network, which
permits only the explicit loopback forward. Neither phase activates, rearms,
changes licensing state, or adjusts the guest clock. Both phases require the
Windows licensing provider to report Licensed, or OOB grace with positive minutes
remaining, and return only the status and remaining minutes as evidence.
Consequently a quarterly cache key alone is not a promise that an offline image
will remain usable for that quarter: an expired cache fails verification. A live
run must establish the observed grace behavior before longer-lived cache reuse
is approved. Do not interpret this failure as permission to reset the evaluation.

## Setup and authentication boundaries

The preparation seed supplies an unpredictable, temporary Administrator password
only to complete unattended setup. There is no automatic login. Setup runs the
configuration as LocalSystem; `sshd` stays disabled and is never started on the
cached disk. An existing SSH key causes preparation to fail.

`SetupComplete.cmd` starts a separate finalizer and returns. The finalizer waits
for its launcher to exit and for Windows' `IMAGE_STATE_COMPLETE`, using registry
change events rather than sleep-based readiness. It replaces the setup password
with another cryptographically random password that is never exported, disables
all local users, and checks the stopped/disabled SSH service and absence of SSH
keys. It records these checks, reports a typed serial event, then shuts down.
The host requires that event and a successful QEMU exit before copying the image.

Answer-file copies are removed to reduce exposure, not as proof of secure erasure. Old
setup password bytes can remain in disk history or setup logs; those bytes no
longer authenticate after password replacement and account disablement. The
base's Administrator remains disabled during verification. The design does not
claim that qcow2 compression erases old sectors.

Verification uses a new copy-on-write overlay and generates fresh Ed25519 client
and server keys in the private work directory. Only the client public key and
the server private key enter the per-run seed. The guest verifies the base checks
again, creates a new probe account in the overlay, restricts key file ACLs to
SYSTEM and Administrators, and starts SSH with public-key-only authentication.
It leaves the firewall enabled and narrows the SSH rule to the virtual gateway.
QEMU binds the SSH forward to `127.0.0.1` only. It exposes neither a host directory
nor a QEMU monitor to the guest.

The client pins the independently generated server public key with an explicit
known-hosts file and strict checking. It does not learn trust from keyscan,
accept-new, or the server's readiness event. It disables agent use, forwarding,
password and keyboard-interactive authentication, and implicit SSH configuration.
After the command check it requests shutdown, requires the separate shutdown
record, and waits for QEMU. Success and failure paths kill/reap any remaining
QEMU process and remove the overlay, sockets, keys and seed files. Cleanup failure
is an error rather than success.

## Tests and live validation

The deterministic suite is included in:

    python3 -m unittest discover -s bench/probe/tests

It injects process failures, guest events, exit codes and interruptions. It checks
measurements, source selection, capacity boundaries, key isolation, host-key
pinning, loopback forwarding and cleanup. Configuration inspections check the
ordering and settings in the PowerShell and answer-file templates; they do not
execute those scripts.

The live probe must separately establish that this ISO's WIM selection matches,
Server 2025 OOBE completes, OpenSSH is present, the registry event wait completes,
the attested base has disabled users without SSH keys, and two distinct overlays
use different host keys while leaving the base unchanged. Record the actual
preparation duration, compressed length, licensing status, SSH result and cleanup
result. A timeout or an unexpected event is a failed probe, not readiness.

## Public interface references

- Unattended specialize commands: <https://learn.microsoft.com/en-us/windows-hardware/customize/desktop/unattend/microsoft-windows-deployment-runsynchronous>
- Setup completion: <https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/add-a-custom-script-to-windows-setup>
- Administrator setup: <https://learn.microsoft.com/en-us/windows-hardware/customize/desktop/unattend/microsoft-windows-shell-setup-useraccounts-administratorpassword>
- OpenSSH defaults: <https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse>
- OpenSSH keys and ACLs: <https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_keymanagement>
- Licensing fields: <https://learn.microsoft.com/en-us/previous-versions/windows/desktop/sppwmi/softwarelicensingproduct>
- QEMU networking and serial channels: <https://www.qemu.org/docs/master/system/invocation.html>
- QEMU image conversion: <https://www.qemu.org/docs/master/tools/qemu-img.html>
- WIM XML extraction: <https://wimlib.net/man1/wiminfo.html>
