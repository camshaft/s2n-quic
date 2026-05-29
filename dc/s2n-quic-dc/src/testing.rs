// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    event,
    path::secret::{stateless_reset::Signer, Map},
    psk::{client, server},
};
use s2n_quic::{provider::tls::Provider, server::Name};
use s2n_quic_core::{crypto::tls::testing::certificates, time::StdClock};
use std::{
    cell::Cell,
    io::{BufWriter, Write as _},
    path::PathBuf,
    process,
    sync::{atomic::AtomicUsize, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(any(test, feature = "testing"))]
use std::sync::{Arc, Mutex};

pub use bach::{ext, rand};

use s2n_quic::provider::tls::default as s2n_quic_tls_prov;

#[cfg(all(test, not(loom)))]
pub mod loom {
    pub use std::{sync, thread};

    pub mod future {
        use core::{
            future::Future,
            task::{Context, Poll},
        };
        use std::sync::Arc;

        pub fn block_on<F: Future>(future: F) -> F::Output {
            struct ThreadWaker(std::thread::Thread);

            impl std::task::Wake for ThreadWaker {
                fn wake(self: Arc<Self>) {
                    self.0.unpark();
                }

                fn wake_by_ref(self: &Arc<Self>) {
                    self.0.unpark();
                }
            }

            let mut future = std::pin::pin!(future);
            let waker = std::task::Waker::from(Arc::new(ThreadWaker(std::thread::current())));
            let mut cx = Context::from_waker(&waker);

            loop {
                match future.as_mut().poll(&mut cx) {
                    Poll::Ready(output) => return output,
                    Poll::Pending => std::thread::park(),
                }
            }
        }
    }

    pub mod hint {
        pub use core::hint::spin_loop;
    }

    pub fn model<F: 'static + FnOnce() -> R, R>(f: F) -> R {
        f()
    }
}

#[cfg(all(test, loom))]
pub use loom;

pub static SNI: OnceLock<Name> = OnceLock::new();

thread_local! {
    static TRACING_DISABLED_DEPTH: Cell<usize> = const { Cell::new(0) };
    static SNAPSHOT_DISABLED_DEPTH: Cell<usize> = const { Cell::new(0) };
    /// Per-test snapshot buffer. Set by `run_sim_with_snapshot`, read by the
    /// snapshot fmt layer's MakeWriter.
    static SNAPSHOT_BUFFER: Cell<Option<Arc<Mutex<SnapshotBuffer>>>> = const { Cell::new(None) };
}

static SNAPSHOT_MODE_DEPTH: AtomicUsize = AtomicUsize::new(0);
const SNAPSHOT_SPILL_THRESHOLD: usize = 256 * 1024;
// Bound retries to avoid pathological looping under heavy contention while
// still giving enough attempts for concurrent test threads.
const MAX_SPILL_FILE_CREATION_ATTEMPTS: usize = 128;

enum SnapshotBufferStorage {
    InMemory(Vec<u8>),
    OnDisk {
        path: PathBuf,
        file: BufWriter<std::fs::File>,
    },
}

struct SnapshotBuffer {
    spill_threshold: usize,
    storage: SnapshotBufferStorage,
}

impl SnapshotBuffer {
    fn new(spill_threshold: usize) -> Self {
        Self {
            spill_threshold,
            storage: SnapshotBufferStorage::InMemory(Vec::new()),
        }
    }

    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let should_spill = if let SnapshotBufferStorage::InMemory(bytes) = &self.storage {
            bytes.len().saturating_add(buf.len()) > self.spill_threshold
        } else {
            false
        };

        if should_spill {
            self.spill_to_disk()?;
        }

        match &mut self.storage {
            SnapshotBufferStorage::InMemory(bytes) => {
                bytes.extend_from_slice(buf);
            }
            SnapshotBufferStorage::OnDisk { file, .. } => {
                file.write_all(buf)?;
            }
        }

        Ok(buf.len())
    }

    fn into_bytes(&mut self) -> std::io::Result<Vec<u8>> {
        match &mut self.storage {
            SnapshotBufferStorage::InMemory(bytes) => Ok(std::mem::take(bytes)),
            SnapshotBufferStorage::OnDisk { file, path } => {
                file.flush()?;
                std::fs::read(path)
            }
        }
    }

    #[cfg(test)]
    fn spill_path(&self) -> Option<PathBuf> {
        match &self.storage {
            SnapshotBufferStorage::OnDisk { path, .. } => Some(path.clone()),
            SnapshotBufferStorage::InMemory(_) => None,
        }
    }

    fn spill_to_disk(&mut self) -> std::io::Result<()> {
        let SnapshotBufferStorage::InMemory(bytes) = &mut self.storage else {
            return Ok(());
        };

        let (file, path) = create_snapshot_spill_file()?;
        let mut file = BufWriter::new(file);
        file.write_all(bytes)?;

        eprintln!(
            "s2n-quic-dc snapshot log buffer exceeded {} bytes; spilling to {}",
            self.spill_threshold,
            path.display()
        );

        self.storage = SnapshotBufferStorage::OnDisk { path, file };
        Ok(())
    }
}

fn create_snapshot_spill_file() -> std::io::Result<(std::fs::File, PathBuf)> {
    // Intentionally create a persistent file in the temp directory so logs remain
    // available for debugging after the test process exits.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let thread_name = sanitize_thread_name(std::thread::current().name().unwrap_or("unnamed"));
    let temp_dir = std::env::temp_dir();

    for attempt in 0..MAX_SPILL_FILE_CREATION_ATTEMPTS {
        let path = temp_dir.join(format!(
            "s2n-quic-dc-test-logs-{thread_name}-{}-{now}-{attempt}.log",
            process::id(),
        ));

        match std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "failed to create unique snapshot spill log file after {MAX_SPILL_FILE_CREATION_ATTEMPTS} attempts (dir={}, thread={thread_name})",
            temp_dir.display()
        ),
    ))
}

fn sanitize_thread_name(name: &str) -> String {
    name.replace([':', '/', '\\', '.', ' '], "_")
}

struct TracingDisabledGuard;

impl TracingDisabledGuard {
    fn enter() -> Self {
        TRACING_DISABLED_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self
    }
}

impl Drop for TracingDisabledGuard {
    fn drop(&mut self) {
        TRACING_DISABLED_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

struct SnapshotDisabledGuard;

impl SnapshotDisabledGuard {
    fn enter() -> Self {
        SNAPSHOT_DISABLED_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self
    }
}

impl Drop for SnapshotDisabledGuard {
    fn drop(&mut self) {
        SNAPSHOT_DISABLED_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Guard that disables tracing for its lifetime.
///
/// Obtain one with [`without_tracing`].  While the guard is live:
/// - `sim` will not produce snapshot output.
/// - The stdout layer suppresses all output.
///
/// Dropping the guard restores the previous state.
pub struct WithoutTracingGuard {
    _depth: Option<TracingDisabledGuard>,
}

/// Guard that suppresses sim snapshot output for its lifetime.
///
/// Obtain one with [`without_snapshots`].  Tracing remains active;
/// only the insta snapshot capture is skipped.  Dropping the guard restores
/// the previous state.
pub struct WithoutSnapshotsGuard {
    _depth: SnapshotDisabledGuard,
}

#[cfg(test)]
struct SnapshotModeGuard;

#[cfg(test)]
impl SnapshotModeGuard {
    fn enter() -> Self {
        SNAPSHOT_MODE_DEPTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self
    }
}

#[cfg(test)]
impl Drop for SnapshotModeGuard {
    fn drop(&mut self) {
        SNAPSHOT_MODE_DEPTH.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[doc(hidden)]
pub fn server_name() -> Name {
    SNI.get_or_init(|| "localhost".into()).clone()
}

pub fn assert_debug<T: core::fmt::Debug>(_v: &T) {}
pub fn assert_send<T: Send>(_v: &T) {}
pub fn assert_sync<T: Sync>(_v: &T) {}
pub fn assert_static<T: 'static>(_v: &T) {}
pub fn assert_async_read<T: tokio::io::AsyncRead>(_v: &T) {}
pub fn assert_async_write<T: tokio::io::AsyncWrite>(_v: &T) {}

pub fn init_tracing() {
    if cfg!(any(miri, fuzzing)) {
        return;
    }

    use std::sync::Once;
    use tracing_subscriber::{layer::SubscriberExt, Layer as _};

    static TRACING: Once = Once::new();

    // make sure this only gets initialized once
    TRACING.call_once(|| {
        let default_level = if std::env::var("CI").is_ok() {
            tracing::Level::INFO
        } else if cfg!(debug_assertions) {
            tracing::Level::DEBUG
        } else {
            tracing::Level::WARN
        };

        let stdout_filter = tracing_subscriber::EnvFilter::builder()
            .with_default_directive(default_level.into())
            .with_env_var("S2N_LOG")
            .from_env()
            .unwrap();

        // Stdout layer: always active unless tracing is explicitly disabled.
        let mut stdout_layer = tracing_subscriber::fmt::layer().event_format(
            tracing_subscriber::fmt::format()
                .with_timer(Uptime::default())
                .compact(),
        );

        // avoid ANSI with agents
        if std::env::var("CLAUDECODE").is_ok() {
            stdout_layer = stdout_layer.with_ansi(false);
        }

        let stdout_layer = stdout_layer
            .with_writer(StdoutWriter)
            .with_filter(stdout_filter);

        // Snapshot layer: only writes when SNAPSHOT_BUFFER is set on this thread.
        // Fixed filter — never reads env vars so snapshot content is deterministic.
        let snapshot_filter = tracing_subscriber::EnvFilter::builder()
            .with_default_directive(tracing::Level::DEBUG.into())
            .parse("")
            .unwrap()
            .add_directive("s2n_quic_dc::metric=trace".parse().unwrap());

        let snapshot_layer = tracing_subscriber::fmt::layer()
            .event_format(
                tracing_subscriber::fmt::format()
                    .with_timer(Uptime::default())
                    .with_target(false)
                    .compact(),
            )
            .with_ansi(false)
            .with_writer(ThreadLocalSnapshotWriter)
            .with_filter(snapshot_filter);

        let subscriber = tracing_subscriber::registry()
            .with(stdout_layer)
            .with(snapshot_layer);

        tracing::subscriber::set_global_default(subscriber)
            .expect("failed to set global tracing subscriber");
    });
}

/// Returns a guard that disables tracing for its lifetime.
///
/// ```rust,ignore
/// let _guard = testing::without_tracing();
/// // ... tracing-free work ...
/// // guard drops here, tracing restored
/// ```
pub fn without_tracing() -> WithoutTracingGuard {
    init_tracing();

    static FORCED: OnceLock<bool> = OnceLock::new();

    if *FORCED.get_or_init(|| std::env::var("S2N_LOG_FORCED").is_ok()) {
        return WithoutTracingGuard { _depth: None };
    }

    WithoutTracingGuard {
        _depth: Some(TracingDisabledGuard::enter()),
    }
}

/// Returns a guard that suppresses sim snapshot output for its lifetime.
///
/// Tracing events continue to be emitted to the normal test subscriber;
/// only the insta snapshot capture is skipped so the output of `sim` is not
/// compared against stored snapshots.
///
/// ⚠️⚠️⚠️ WARNING: avoid using this unless snapshotting is genuinely impractical.
/// In this repository, disabling snapshots is only allowed when:
/// 1) the snapshot would be unreasonably large, or
/// 2) the test runs a multi-sim harness across varying inputs.
///
/// If neither condition applies, snapshots should remain enabled to detect regressions.
///
/// ```rust,ignore
/// let _guard = testing::without_snapshots();
/// testing::sim(|| { ... }); // runs, but no snapshot is taken
/// ```
pub fn without_snapshots() -> WithoutSnapshotsGuard {
    WithoutSnapshotsGuard {
        _depth: SnapshotDisabledGuard::enter(),
    }
}

#[derive(Default)]
struct Uptime(tracing_subscriber::fmt::time::SystemTime);

// Generate the timestamp from the testing IO provider rather than wall clock.
impl tracing_subscriber::fmt::time::FormatTime for Uptime {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        if bach::is_active() {
            write!(
                w,
                "{} [{}]",
                bach::time::Instant::now(),
                bach::group::current().name()
            )
        } else {
            self.0.format_time(w)
        }
    }
}

/// Runs a function in a deterministic, discrete event simulation environment
#[track_caller]
pub fn sim(f: impl FnOnce()) {
    init_tracing();

    #[cfg(test)]
    {
        if !is_tracing_disabled() && !is_snapshots_disabled() && !bolero::is_active() {
            return run_sim_with_snapshot(f);
        }
    }

    run_sim(f);
}

fn run_sim(f: impl FnOnce()) {
    // 1ms RTT
    let net_delay = Duration::from_micros(500);
    let queues = bach::environment::net::queue::Fixed::default().with_net_latency(net_delay);
    let mut rt = bach::environment::default::Runtime::new().with_net_queues(Some(Box::new(queues)));
    rt.run(f);
}

#[cfg(test)]
fn is_tracing_disabled() -> bool {
    TRACING_DISABLED_DEPTH.with(|depth| depth.get() > 0)
}

#[cfg(test)]
fn is_snapshots_disabled() -> bool {
    SNAPSHOT_DISABLED_DEPTH.with(|depth| depth.get() > 0)
}

#[cfg(test)]
#[track_caller]
fn run_sim_with_snapshot(f: impl FnOnce()) {
    let snapshot_name = std::thread::current()
        .name()
        .map(sanitize_thread_name)
        .unwrap_or_else(|| "unknown".into());

    let buffer = Arc::new(Mutex::new(SnapshotBuffer::new(SNAPSHOT_SPILL_THRESHOLD)));
    SNAPSHOT_BUFFER.with(|cell| cell.set(Some(buffer.clone())));
    let _snapshot_mode_guard = SnapshotModeGuard::enter();

    run_sim(f);

    // Clear the buffer so it doesn't leak into subsequent tests on this thread.
    SNAPSHOT_BUFFER.with(|cell| cell.set(None));

    let bytes = {
        let mut buffer = buffer.lock().unwrap();
        buffer.into_bytes().unwrap_or_else(|error| {
            let source = buffer
                .spill_path()
                .map(|path| format!("disk spill file {}", path.display()))
                .unwrap_or_else(|| "in-memory snapshot buffer".into());
            format!("failed to collect snapshot logs from {source}: {error}").into_bytes()
        })
    };
    let logs = normalize_snapshot_logs(String::from_utf8_lossy(&bytes).into_owned());

    insta::with_settings!({prepend_module_to_snapshot => false}, {
        insta::assert_snapshot!(snapshot_name, logs);
    });
}

#[cfg(test)]
fn normalize_snapshot_logs(logs: String) -> String {
    let mut normalized = String::with_capacity(logs.len());

    for segment in logs.split_inclusive('\n') {
        let (line, newline) = segment
            .strip_suffix('\n')
            .map(|line| (line, "\n"))
            .unwrap_or((segment, ""));
        normalized.push_str(&trim_rust_location_suffix(line));
        normalized.push_str(newline);
    }

    normalized
}

#[cfg(test)]
fn trim_rust_location_suffix(line: &str) -> String {
    let Some((prefix, suffix)) = line.rsplit_once(".rs:") else {
        return line.into();
    };

    let suffix_has_only_numbers = suffix
        .split(':')
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));

    if suffix_has_only_numbers {
        format!("{prefix}.rs")
    } else {
        line.into()
    }
}

/// MakeWriter for the stdout layer. Produces a sink when tracing is disabled.
struct StdoutWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for StdoutWriter {
    type Writer = StdoutWriterGuard;

    fn make_writer(&'a self) -> Self::Writer {
        let active = !TRACING_DISABLED_DEPTH.with(|depth| depth.get() > 0);
        StdoutWriterGuard { active }
    }
}

struct StdoutWriterGuard {
    active: bool,
}

impl std::io::Write for StdoutWriterGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.active {
            std::io::stderr().write(buf)
        } else {
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.active {
            std::io::stderr().flush()
        } else {
            Ok(())
        }
    }
}

/// MakeWriter for the snapshot layer. Writes to the per-test SNAPSHOT_BUFFER
/// thread-local when set, otherwise discards.
struct ThreadLocalSnapshotWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ThreadLocalSnapshotWriter {
    type Writer = ThreadLocalSnapshotWriterGuard;

    fn make_writer(&'a self) -> Self::Writer {
        let buffer = SNAPSHOT_BUFFER.with(|cell| {
            // SAFETY: we borrow the Option, clone the Arc if present, then put it back.
            let opt = cell.take();
            let cloned = opt.clone();
            cell.set(opt);
            cloned
        });
        ThreadLocalSnapshotWriterGuard { buffer }
    }
}

struct ThreadLocalSnapshotWriterGuard {
    buffer: Option<Arc<Mutex<SnapshotBuffer>>>,
}

impl std::io::Write for ThreadLocalSnapshotWriterGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(buffer) = &self.buffer {
            buffer.lock().unwrap().write(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{sanitize_thread_name, SnapshotBuffer};

    #[test]
    fn snapshot_buffer_stays_in_memory_below_threshold() {
        let mut buffer = SnapshotBuffer::new(32);
        buffer.write(b"hello").unwrap();
        buffer.write(b" world").unwrap();

        assert!(buffer.spill_path().is_none());
        assert_eq!(buffer.into_bytes().unwrap(), b"hello world");
    }

    #[test]
    fn snapshot_buffer_spills_to_disk_at_threshold() {
        let mut buffer = SnapshotBuffer::new(8);
        buffer.write(b"1234").unwrap();
        buffer.write(b"5678").unwrap();
        buffer.write(b"9").unwrap();

        let spill_path = buffer.spill_path().expect("expected spill path");
        assert!(spill_path.exists());
        assert!(spill_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(&sanitize_thread_name(
                std::thread::current().name().unwrap_or("unnamed")
            )));
        let bytes = buffer.into_bytes().unwrap();
        assert!(bytes.starts_with(b"12345678"));
        assert_eq!(bytes, b"123456789");

        std::fs::remove_file(spill_path).unwrap();
    }
}

#[derive(Clone, Default)]
pub struct NoopSubscriber;

// Need to implement both s2n-quic-dc::event::Subscriber and s2n-quic-core::event::Subscriber
// to fulfill the trait bounds for both client::Provider and server::Provider
impl crate::event::Subscriber for NoopSubscriber {
    type ConnectionContext = ();

    fn create_connection_context(
        &self,
        _meta: &event::api::ConnectionMeta,
        _info: &event::api::ConnectionInfo,
    ) -> Self::ConnectionContext {
    }
}

impl s2n_quic_core::event::Subscriber for NoopSubscriber {
    type ConnectionContext = ();

    fn create_connection_context(
        &mut self,
        _meta: &s2n_quic_core::event::api::ConnectionMeta,
        _info: &s2n_quic_core::event::api::ConnectionInfo,
    ) -> Self::ConnectionContext {
    }
}

#[derive(Default, Clone)]
pub struct TestTlsProvider {}

impl Provider for TestTlsProvider {
    type Server = s2n_quic_tls_prov::Server;
    type Client = s2n_quic_tls_prov::Client;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn start_server(self) -> Result<Self::Server, Self::Error> {
        let server = s2n_quic_tls_prov::Server::builder()
            .with_application_protocols(["h3"].iter())?
            .with_certificate(certificates::CERT_PEM, certificates::KEY_PEM)?
            .build()?;
        Ok(server)
    }

    fn start_client(self) -> Result<Self::Client, Self::Error> {
        let client = s2n_quic_tls_prov::Client::builder()
            .with_application_protocols(["h3"].iter())?
            .with_certificate(certificates::CERT_PEM)?
            .build()?;
        Ok(client)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Pair {
    pub client_mtu: Option<u16>,
    pub server_mtu: Option<u16>,
}

impl Pair {
    fn server(&self) -> server::Builder<impl s2n_quic_core::event::Subscriber> {
        let mut server = server::Provider::builder();

        if let Some(mtu) = self.server_mtu {
            server = server.with_mtu(mtu);
        }

        server
    }

    fn client(&self) -> client::Builder<impl s2n_quic_core::event::Subscriber> {
        let mut client = client::Provider::builder();

        if let Some(mtu) = self.client_mtu {
            client = client.with_mtu(mtu);
        }

        // Don't wait after previous handshake before trying another one.
        //
        // Primarily this is needed for restart tests, which expect to recover immediately. In
        // production we don't expect to have *just* handshaked with a peer that's restarting (or
        // at least that's uncommon) and peers rarely undergo e.g. deployment in less than 1
        // minute. So not generally an issue there.
        client = client.with_success_jitter(Duration::ZERO);

        client
    }

    pub async fn build(self) -> (client::Provider, server::Provider) {
        init_tracing();

        let tls_materials_provider = TestTlsProvider {};
        let test_event_subscriber = NoopSubscriber {};

        let server = self
            .server()
            .start(
                "[::1]:0".parse().unwrap(),
                tls_materials_provider.clone(),
                test_event_subscriber.clone(),
                Map::new(
                    Signer::new(b"default"),
                    50_000,
                    false,
                    StdClock::default(),
                    test_event_subscriber.clone(),
                ),
            )
            .await
            .unwrap();

        let client = self
            .client()
            .start(
                "[::]:0".parse().unwrap(),
                Map::new(
                    Signer::new(b"default"),
                    50_000,
                    false,
                    StdClock::default(),
                    test_event_subscriber.clone(),
                ),
                tls_materials_provider,
                test_event_subscriber,
                server_name(),
            )
            .unwrap();

        (client, server)
    }

    pub fn build_sync(self) -> (client::Provider, server::Provider) {
        init_tracing();

        let tls_materials_provider = TestTlsProvider {};
        let test_event_subscriber = NoopSubscriber {};

        let server = self
            .server()
            .start_blocking(
                "[::1]:0".parse().unwrap(),
                tls_materials_provider.clone(),
                test_event_subscriber.clone(),
                Map::new(
                    Signer::new(b"default"),
                    50_000,
                    false,
                    StdClock::default(),
                    test_event_subscriber.clone(),
                ),
            )
            .unwrap();

        let client = self
            .client()
            .start(
                "[::]:0".parse().unwrap(),
                Map::new(
                    Signer::new(b"default"),
                    50_000,
                    false,
                    StdClock::default(),
                    test_event_subscriber.clone(),
                ),
                tls_materials_provider,
                test_event_subscriber,
                server_name(),
            )
            .unwrap();

        (client, server)
    }
}

pub fn pair_sync() -> (client::Provider, server::Provider) {
    Pair::default().build_sync()
}

pub fn send_busy_poll() -> crate::busy_poll::Pool {
    static POOL: BusyPool = BusyPool::new();
    POOL.get()
}

pub fn recv_busy_poll() -> crate::busy_poll::Pool {
    static POOL: BusyPool = BusyPool::new();
    POOL.get()
}

struct BusyPool(std::sync::OnceLock<crate::busy_poll::Pool>);

impl BusyPool {
    const fn new() -> Self {
        Self(std::sync::OnceLock::new())
    }

    fn get(&self) -> crate::busy_poll::Pool {
        self.0
            .get_or_init(|| {
                let mut handles = vec![];
                for worker_id in 0..2 {
                    let (handle, runner) = crate::busy_poll::Handle::new(worker_id);
                    std::thread::spawn(move || runner.run());
                    handles.push(handle);
                }
                handles.into()
            })
            .clone()
    }
}
