// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Direct (unbuffered) file IO primitive — a port of Membrain's `core-fs-direct`.
//!
//! Provides page-aligned buffer allocation and an [`O_DIRECT`](open_direct_file)/`F_NOCACHE` file
//! handle with positional `read_at`/`write_at`/`sync`. The blocking-syscall backend
//! ([`crate::fs::backend::syscall`]) runs these on its bounded worker pool.
//!
//! `O_DIRECT` requires the buffer pointer, the file offset, and the length to all be aligned to the
//! device block size (here [`ALIGNMENT`] = 4 KiB). [`AlignedBuf`] guarantees the pointer and a
//! page-rounded capacity; callers are responsible for aligned offsets/lengths. On tmpfs (where
//! `O_DIRECT` is rejected) and on macOS the open path falls back appropriately.

use std::{
    fs::OpenOptions,
    io,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};

/// Device block alignment for direct IO. 4 KiB covers every common page/sector size.
pub const ALIGNMENT: usize = 4096;

/// Round `n` up to the next multiple of [`ALIGNMENT`].
#[inline]
pub fn align_up(n: usize) -> usize {
    (n + ALIGNMENT - 1) & !(ALIGNMENT - 1)
}

/// A page-aligned heap buffer for direct IO. The allocation pointer is aligned to [`ALIGNMENT`] and
/// the capacity is rounded up to a whole number of pages, so a read/write of an aligned length is a
/// valid `O_DIRECT` transfer.
///
/// This is the caller-owned buffer for **zero-copy** direct IO: the application fills it (write) or
/// receives into it (read) with no intermediate copy in the data path. Its pointer is always
/// [`ALIGNMENT`]-aligned and its `aligned_len` is a whole number of pages, satisfying two of
/// `O_DIRECT`'s three constraints; the third (a block-aligned file offset) is the caller's
/// responsibility and is validated at submit time.
pub struct AlignedBuf {
    ptr: std::ptr::NonNull<u8>,
    /// Logical length exposed to callers (may be < `cap`).
    len: usize,
    /// Page-rounded allocation size.
    cap: usize,
}

impl core::fmt::Debug for AlignedBuf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AlignedBuf")
            .field("len", &self.len)
            .field("cap", &self.cap)
            .finish()
    }
}

// SAFETY: `AlignedBuf` owns its allocation exclusively; the bytes are plain data.
unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// Allocate a zeroed buffer whose capacity is `align_up(len)` and whose logical length is `len`.
    pub fn new(len: usize) -> Self {
        let cap = align_up(len.max(1));
        let layout = std::alloc::Layout::from_size_align(cap, ALIGNMENT)
            .expect("valid aligned layout");
        // SAFETY: layout has non-zero size (cap >= ALIGNMENT) and valid alignment.
        let raw = unsafe { std::alloc::alloc_zeroed(layout) };
        let ptr = std::ptr::NonNull::new(raw)
            .unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        Self { ptr, len, cap }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The page-rounded capacity (the size a direct read/write actually transfers).
    #[inline]
    pub fn aligned_len(&self) -> usize {
        self.cap
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `len` initialized bytes (alloc_zeroed initialized all `cap`).
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// The logical-length mutable slice, for an in-place direct read destination (zero-copy).
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` is valid for `len` zero-initialized bytes and we hold `&mut self`.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// The full page-rounded slice, for an aligned direct write source.
    #[inline]
    pub fn as_aligned_slice(&self) -> &[u8] {
        // SAFETY: all `cap` bytes were zero-initialized at allocation.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.cap) }
    }

    /// The full page-rounded mutable slice, for an aligned direct read destination.
    #[inline]
    pub fn as_aligned_mut(&mut self) -> &mut [u8] {
        // SAFETY: all `cap` bytes were zero-initialized and we hold `&mut self`.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.cap) }
    }

    /// Set the logical length (must be `<= aligned capacity`).
    #[inline]
    pub fn set_len(&mut self, len: usize) {
        debug_assert!(len <= self.cap);
        self.len = len;
    }

    /// True if the pointer is [`ALIGNMENT`]-aligned (always true for `AlignedBuf`).
    #[inline]
    pub fn is_aligned(&self) -> bool {
        (self.ptr.as_ptr() as usize).is_multiple_of(ALIGNMENT)
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = std::alloc::Layout::from_size_align(self.cap, ALIGNMENT)
            .expect("valid aligned layout");
        // SAFETY: `ptr`/`cap`/alignment match the original allocation.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), layout) };
    }
}

/// Options for opening a direct-IO file.
pub struct Options {
    /// Truncate the file on open.
    pub truncate: bool,
    /// Pre-size the file to this many bytes (use 0 to leave the size unchanged).
    pub size: u64,
    /// Open with `O_DIRECT`/`F_NOCACHE`. Set false for tmpfs/test files where direct IO is rejected.
    pub direct: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            truncate: false,
            size: 0,
            direct: true,
        }
    }
}

/// A direct-IO file handle with positional read/write. Cheap to clone the path; the underlying
/// `std::fs::File` is shared by the worker pool via the fd it exposes.
pub struct File {
    inner: std::fs::File,
    path: PathBuf,
}

impl File {
    /// Open `path` per `options`, optionally in direct (unbuffered) mode.
    pub fn open(path: impl AsRef<Path>, options: Options) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let inner = open_file(&path, options.truncate, options.direct)?;
        if options.size > 0 {
            inner.set_len(options.size)?;
        }
        Ok(Self { inner, path })
    }

    /// The raw fd, for handing to a backend (e.g. an `IoOp`'s `fd` field).
    #[inline]
    pub fn raw_fd(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.inner.as_raw_fd()
    }

    #[inline]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read exactly `buf.len()` bytes at `offset`.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.inner.read_exact_at(buf, offset)
    }

    /// Write all of `buf` at `offset`.
    pub fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.inner.write_all_at(buf, offset)
    }

    /// Flush file data + metadata to the device.
    pub fn sync(&self) -> io::Result<()> {
        self.inner.sync_all()
    }

    /// Flush file data (not necessarily metadata) to the device.
    pub fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data()
    }
}

/// Open a file, optionally in direct (unbuffered) mode. On Linux uses `O_DIRECT` (skipped on tmpfs,
/// which rejects it); on macOS sets `F_NOCACHE` after open.
fn open_file(path: &Path, truncate: bool, direct: bool) -> io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(truncate);

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if direct && !is_tmpfs(path).unwrap_or(true) {
            options.custom_flags(libc::O_DIRECT);
        }
        options.open(path)
    }

    #[cfg(target_os = "macos")]
    {
        let file = options.open(path)?;
        if direct {
            use std::os::fd::AsRawFd;
            // SAFETY: `file` owns a valid fd for the duration of this call.
            let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) };
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(file)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = direct;
        options.open(path)
    }
}

/// True if `path`'s parent directory is on tmpfs (where `O_DIRECT` is unsupported).
#[cfg(target_os = "linux")]
fn is_tmpfs(path: &Path) -> io::Result<bool> {
    use std::{ffi::CString, mem::MaybeUninit};

    let dir = path.parent().unwrap_or(path);
    let cstr = CString::new(dir.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains null byte"))?;
    let mut statfs = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `cstr` is a valid C string; `statfs` is a valid out pointer.
    let ret = unsafe { libc::statfs(cstr.as_ptr(), statfs.as_mut_ptr()) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `statfs` was initialized by a successful `statfs` call.
    let statfs = unsafe { statfs.assume_init() };
    Ok(statfs.f_type == libc::TMPFS_MAGIC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_buf_is_aligned_and_page_rounded() {
        let buf = AlignedBuf::new(100);
        assert!(buf.is_aligned());
        assert_eq!(buf.len(), 100);
        assert_eq!(buf.aligned_len(), ALIGNMENT);
        let buf = AlignedBuf::new(ALIGNMENT + 1);
        assert_eq!(buf.aligned_len(), 2 * ALIGNMENT);
    }

    #[test]
    fn align_up_rounds() {
        assert_eq!(align_up(0), 0);
        assert_eq!(align_up(1), ALIGNMENT);
        assert_eq!(align_up(ALIGNMENT), ALIGNMENT);
        assert_eq!(align_up(ALIGNMENT + 1), 2 * ALIGNMENT);
    }

    #[test]
    fn buffered_read_write_roundtrip() {
        // Use a temp file in buffered mode (no O_DIRECT) so the test is portable across filesystems.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("s2n-dc-direct-test-{}", std::process::id()));
        let file = File::open(
            &path,
            Options {
                truncate: true,
                size: 0,
                direct: false,
            },
        )
        .unwrap();

        let data = b"hello direct io world";
        file.write_at(data, 0).unwrap();
        file.sync().unwrap();

        let mut buf = vec![0u8; data.len()];
        file.read_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, data);

        let _ = std::fs::remove_file(&path);
    }
}
