// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! io_uring zero-copy-RX (ZCRX) capability probe — a GO/NO-GO for whether a NIC driver exposes the
//! io_uring `register_ifq` (IORING_REGISTER_ZCRX_IFQ) netdev queue API, which the RECV_ZC lever needs.
//!
//! This is DISTINCT from AF_XDP zero-copy (e.g. amzn-drivers PR#378): AF_XDP-ZC support does NOT imply
//! io_uring-zcrx support — they use different netdev queue APIs. Our recv path is io_uring-based, so
//! only io_uring-zcrx (this probe) is usable directly.
//!
//! Run on the target host (needs the NIC to be up; may need a steered rx queue):
//!   cargo run -p s2n-quic-dc --example zcrx_probe -- <iface> <rxq>
//! e.g. `cargo run -p s2n-quic-dc --example zcrx_probe -- eth0 0`
//!
//! Verdict:
//!   Ok                  => GO   (io_uring-zcrx works on this NIC; build + deploy RECV_ZC).
//!   EOPNOTSUPP (95)     => NO-GO (driver does not expose the io_uring-zcrx queue API).
//!   EINVAL (22) / other => INCONCLUSIVE — this probe's hand-built registration may be off (kernel
//!                          validates args before the driver check). Cross-check with liburing's
//!                          reference `examples/io_uring-zcrx.c` before treating it as NO-GO.
//!
//! Only `Ok` and `EOPNOTSUPP` are trustworthy from this hand-rolled registration; a false EINVAL must
//! NOT drive the AF_XDP re-architecture decision.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("zcrx_probe: linux only");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn main() {
    use io_uring::{
        types::{io_uring_region_desc, io_uring_zcrx_area_reg, io_uring_zcrx_ifq_reg},
        IoUring,
    };

    // ── args ────────────────────────────────────────────────────────────────
    let mut args = std::env::args().skip(1);
    let iface = args.next().unwrap_or_else(|| "eth0".to_string());
    let rxq: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);

    // ── resolve ifindex ───────────────────────────────────────────────────────
    let cstr = std::ffi::CString::new(iface.clone()).expect("iface name");
    // SAFETY: `cstr` is a valid NUL-terminated C string for the call's duration.
    let if_idx = unsafe { libc::if_nametoindex(cstr.as_ptr()) };
    if if_idx == 0 {
        eprintln!(
            "zcrx_probe: if_nametoindex({iface}) failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(2);
    }

    // ── ring: io_uring-zcrx REQUIRES DEFER_TASKRUN (+ SINGLE_ISSUER + the COOP base). A plain ring
    // makes register_ifq fail EINVAL for the setup, not the driver — so set them or we'd read a false
    // NO-GO. ──────────────────────────────────────────────────────────────────
    let ring: IoUring = match IoUring::builder()
        .setup_single_issuer()
        .setup_coop_taskrun()
        .setup_defer_taskrun()
        .build(64)
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "zcrx_probe: ring build (single_issuer|defer_taskrun) failed: {e} — kernel < 6.1?"
            );
            std::process::exit(2);
        }
    };

    // ── page size ─────────────────────────────────────────────────────────────
    // SAFETY: plain libc query.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as usize;
    let page_align = |n: usize| (n + page - 1) & !(page - 1);

    // ── zcrx AREA: the DMA buffer pool (page-aligned anonymous memory the kernel pins). 16 MiB. ──
    let area_len = 16 * 1024 * 1024;
    let area_ptr = mmap_anon(area_len);

    // ── refill REGION: holds the refill ring (rq_entries * io_uring_zcrx_rqe(16B) + a header page). ──
    let rq_entries: u32 = 1024; // power of two
    let region_len = page_align(rq_entries as usize * 16 + page);
    let region_ptr = mmap_anon(region_len);

    // ── build the registration structs ────────────────────────────────────────
    let mut area = unsafe { std::mem::zeroed::<io_uring_zcrx_area_reg>() };
    area.addr = area_ptr as u64;
    area.len = area_len as u64;

    let mut region = unsafe { std::mem::zeroed::<io_uring_region_desc>() };
    region.user_addr = region_ptr as u64;
    region.size = region_len as u64;
    region.flags = io_uring::types::IORING_MEM_REGION_TYPE_USER as u32;

    let mut reg = unsafe { std::mem::zeroed::<io_uring_zcrx_ifq_reg>() };
    reg.if_idx = if_idx;
    reg.if_rxq = rxq;
    reg.rq_entries = rq_entries;
    reg.area_ptr = (&area as *const io_uring_zcrx_area_reg) as u64;
    reg.region_ptr = (&region as *const io_uring_region_desc) as u64;

    eprintln!(
        "zcrx_probe: iface={iface} if_idx={if_idx} rxq={rxq} rq_entries={rq_entries} \
         area={area_len}B region={region_len}B page={page}"
    );

    // ── the probe ──────────────────────────────────────────────────────────────
    let result = ring.submitter().register_ifq(&reg);

    // keep the mmaps alive across the syscall
    std::hint::black_box((area_ptr, region_ptr));

    match result {
        Ok(()) => {
            println!("GO: io_uring-zcrx register_ifq SUCCEEDED on {iface} rxq {rxq} — driver supports io_uring zero-copy RX. Build + deploy RECV_ZC.");
            std::process::exit(0);
        }
        Err(e) => {
            let errno = e.raw_os_error().unwrap_or(-1);
            match errno {
                95 => {
                    // EOPNOTSUPP
                    println!("NO-GO: register_ifq -> EOPNOTSUPP (95) on {iface} rxq {rxq} — driver does NOT expose the io_uring-zcrx queue API. io_uring RECV_ZC is not viable on this NIC.");
                    std::process::exit(1);
                }
                22 => {
                    // EINVAL
                    println!("INCONCLUSIVE: register_ifq -> EINVAL (22). The kernel validated args before (or instead of) the driver check — this hand-built registration may be malformed (rq_entries/region size/rxq/area). Do NOT treat as NO-GO. Cross-check with liburing's examples/io_uring-zcrx.c (reference-correct). rxq {rxq} valid? try another queue or `ethtool -l {iface}`.");
                    std::process::exit(3);
                }
                other => {
                    println!("INCONCLUSIVE: register_ifq -> errno {other} ({e}). Not a clean GO/NO-GO; cross-check with liburing's io_uring-zcrx example.");
                    std::process::exit(3);
                }
            }
        }
    }
}

/// Anonymous, private, page-aligned mmap of `len` bytes (the kernel pins these for zcrx). Leaked for
/// the process lifetime — this is a one-shot probe.
#[cfg(target_os = "linux")]
fn mmap_anon(len: usize) -> *mut libc::c_void {
    // SAFETY: standard anonymous mmap; we check for MAP_FAILED.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        eprintln!(
            "zcrx_probe: mmap({len}) failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(2);
    }
    p
}
