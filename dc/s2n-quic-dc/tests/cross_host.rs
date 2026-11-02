// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! One end of a stream exchange, run as a process of its own over kernel UDP sockets, so that the
//! two ends can run on two hosts with packet loss and a link drop on the path between them.
//!
//! `cross_host_end` runs one end, the server or the client, and runs only where its caller passes
//! `--ignored`: a host that runs the two ends starts each one with
//! `--ignored --exact cross_host_end` and the environment below, and applies the schedule's
//! faults on the path between them. `loopback` runs both ends as child processes of this test
//! binary on the loopback interface, so that each CI run builds and runs them. The loopback
//! interface doesn't lose packets or take its link down, so that test runs the schedule's window
//! without a fault.
//!
//! The environment of `cross_host_end`:
//!
//! - `DC_CROSS_HOST_ROLE`: `server` or `client`.
//! - `DC_CROSS_HOST_LISTEN` (server): the address the server takes handshakes on. Its data
//!   sockets bind to the same IP on an ephemeral port, which the handshake advertises.
//! - `DC_CROSS_HOST_PEER` (client): the server's handshake address.
//! - `DC_CROSS_HOST_RESULTS`: the file the end appends its JSON lines to.
//! - `DC_CROSS_HOST_LOSS_PERCENT`, `DC_CROSS_HOST_DROP_START_MS` and `DC_CROSS_HOST_DROP_MS`: the
//!   fault schedule that the host applies and that every JSON line states. The ends don't apply
//!   it.
//! - `DC_CROSS_HOST_RUN_MS`, `DC_CROSS_HOST_STREAM_INTERVAL_MS` and `DC_CROSS_HOST_STREAM_BYTES`
//!   (client): how long the client starts streams for, how often it starts one, and the length of
//!   each request, which the server writes back. Where the schedule has a drop, the run has to
//!   outlast the drop's end by the crate's idle timeout and its dead-peer cooldown together
//!   (`endpoint::DEFAULT_IDLE_TIMEOUT` and `endpoint::DEFAULT_DEAD_PEER_COOLDOWN`): a stream that
//!   starts just before the drop ends can stay open until the idle timeout declares the peer dead,
//!   the client's streams then fail until the cooldown ends, and the client needs a completed
//!   stream that starts after the drop ends and one more after it. The client checks only that the
//!   run ends after the drop does.
//!
//! The schedule's times count from the start of the client end. The client runs one stream at a
//! time and asserts that each stream that ended before the drop started completed, that a stream
//! started after the drop ended completed, which is its reconnection, and that each stream after
//! that one completed. It writes one JSON line for each stream, one for the reconnection where
//! the schedule has a drop, and one for the stream that ends the run. The server writes one line
//! when it listens, one for each stream it writes back, and one when the client ends the run.
//!
//! Each end builds the stream endpoint on the production socket configuration
//! (`endpoint::socket::Config`), as the dc-tester load tester does.

use bytes::{Bytes, BytesMut};
use s2n_quic::{
    provider::{event::default::Subscriber as HandshakeEvents, tls::default as tls},
    server::Name,
};
use s2n_quic_core::{crypto::tls::testing::certificates, varint::VarInt};
use s2n_quic_dc::{
    acceptor,
    credit,
    endpoint::{self, Budgets, Endpoint, RecvBackend, WorkerLayout},
    event,
    path::secret::{stateless_reset::Signer, Map},
    psk,
    runtime,
    socket::{pool::Pool, rate::Rate, LocalAddr},
    stream::{self, Priority, Stream},
};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    io::{self, BufRead, BufReader, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

/// The acceptor that takes the client's request streams.
const ECHO_ACCEPTOR: VarInt = VarInt::from_u8(1);
/// The acceptor that takes the client's last stream, which ends the server's run.
const END_ACCEPTOR: VarInt = VarInt::from_u8(2);
/// The server name of the handshake's test certificate.
const SERVER_NAME: &str = "localhost";

/// The schedule `loopback` runs both ends under: a link drop of 1 ms at the client end's start.
/// The window passes without a fault, since the loopback interface doesn't lose packets or take
/// its link down, so no stream waits out the idle timeout or the dead-peer cooldown and the run
/// doesn't need to outlast the drop by them. The client's times count from before it builds its
/// endpoint and makes its first handshake, so with the drop at the start, `check` needs only two
/// streams anywhere in the run: the reconnection, the first stream that starts at or after 1 ms
/// and completes, and one more. A drop later in the run would need streams after it, which a slow
/// setup, as on a sanitizer or coverage build, can push past the run's end.
const LOOPBACK_SCHEDULE: [(&str, &str); 3] = [
    ("DC_CROSS_HOST_LOSS_PERCENT", "0"),
    ("DC_CROSS_HOST_DROP_START_MS", "0"),
    ("DC_CROSS_HOST_DROP_MS", "1"),
];
/// The length of each request of the client end in `loopback`.
const LOOPBACK_STREAM_BYTES: &str = "16384";
/// The client end's run in `loopback`: a stream each 100 ms for 2.5 s from the client end's start.
const LOOPBACK_CLIENT: [(&str, &str); 3] = [
    ("DC_CROSS_HOST_RUN_MS", "2500"),
    ("DC_CROSS_HOST_STREAM_INTERVAL_MS", "100"),
    ("DC_CROSS_HOST_STREAM_BYTES", LOOPBACK_STREAM_BYTES),
];

#[test]
#[ignore = "runs one end of a cross-host exchange, which `loopback` or a two-host run starts"]
fn cross_host_end() {
    let started = Instant::now();
    init_logs();
    let schedule = Schedule::from_env();
    let role = var("DC_CROSS_HOST_ROLE");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let local = tokio::task::LocalSet::new();
    match role.as_str() {
        "server" => local.block_on(&rt, run_server(schedule)),
        "client" => local.block_on(&rt, run_client(schedule, started)),
        other => panic!("DC_CROSS_HOST_ROLE={other} is neither server nor client"),
    }
}

/// Runs the server end and the client end as child processes of this test binary on the loopback
/// interface, and checks the lines each end wrote. The loopback interface doesn't lose packets or
/// take its link down, so every stream completes, and the client's reconnection after the
/// schedule's window follows zero failed streams.
#[test]
fn loopback() {
    let name = format!("dc-cross-host-{}", std::process::id());
    let dir = std::env::temp_dir().join(name);
    std::fs::create_dir_all(&dir).expect("the results directory");
    let server_results = dir.join("server.jsonl");
    let client_results = dir.join("client.jsonl");

    let mut server = ServerEnd(
        end_command("server", &server_results)
            .env("DC_CROSS_HOST_LISTEN", "127.0.0.1:0")
            .stdout(Stdio::piped())
            .spawn()
            .expect("start the server end"),
    );
    let mut output = BufReader::new(server.0.stdout.take().expect("the server end's stdout"));
    let peer = listening_addr(&mut output);
    // Copy the rest of the server's output to stderr, so that the server doesn't wait on a full
    // pipe.
    let copy = std::thread::spawn(move || io::copy(&mut output, &mut io::stderr()));

    let client = end_command("client", &client_results)
        .env("DC_CROSS_HOST_PEER", peer.to_string())
        .envs(LOOPBACK_CLIENT)
        .status()
        .expect("run the client end");
    assert!(client.success(), "the client end failed: {client}");
    let server_status = server.0.wait().expect("wait for the server end");
    assert!(
        server_status.success(),
        "the server end failed: {server_status}"
    );
    let copied = copy.join().expect("the output copy thread");
    copied.expect("the server end's output");

    let schedule = json!({ "loss_percent": 0.0, "drop_start_ms": 0, "drop_ms": 1 });

    let client_lines = read_lines(&client_results);
    for line in &client_lines {
        assert_eq!(line["role"], "client", "{line}");
        assert_eq!(line["schedule"], schedule, "{line}");
    }
    let client_streams = events(&client_lines, "stream");
    assert!(!client_streams.is_empty(), "the client ran zero streams");
    for line in &client_streams {
        assert_eq!(line["outcome"], "ok", "{line}");
        assert_eq!(line["bytes"].to_string(), LOOPBACK_STREAM_BYTES, "{line}");
        assert!(line["phase"].is_string(), "{line}");
    }
    let reconnects = events(&client_lines, "reconnect");
    assert_eq!(reconnects.len(), 1, "{client_lines:?}");
    assert_eq!(reconnects[0]["reconnected"], true, "{}", reconnects[0]);
    assert_eq!(reconnects[0]["failed_streams"], 0, "{}", reconnects[0]);
    let client_end = events(&client_lines, "end");
    assert_eq!(client_end.len(), 1, "{client_lines:?}");
    assert_eq!(client_end[0]["outcome"], "ok", "{}", client_end[0]);
    assert_eq!(client_end[0]["streams"], client_streams.len());

    let server_lines = read_lines(&server_results);
    for line in &server_lines {
        assert_eq!(line["role"], "server", "{line}");
        assert_eq!(line["schedule"], schedule, "{line}");
    }
    assert_eq!(events(&server_lines, "listening").len(), 1);
    let server_streams = events(&server_lines, "stream");
    assert_eq!(server_streams.len(), client_streams.len());
    for line in &server_streams {
        assert_eq!(line["outcome"], "ok", "{line}");
    }
    let server_end = events(&server_lines, "end");
    assert_eq!(server_end.len(), 1, "{server_lines:?}");
    assert_eq!(server_end[0]["outcome"], "ok", "{}", server_end[0]);
    assert_eq!(server_end[0]["streams"], client_streams.len());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A command that runs `cross_host_end` of this test binary as `role` under the loopback
/// schedule, which appends its lines to `results`. The end runs one test at a time, so libtest
/// writes `test cross_host_end ... ` at the start of the line that the end's own output follows,
/// as it does on a host with one CPU, and `listening_addr` reads the server's line after it.
fn end_command(role: &str, results: &Path) -> Command {
    let exe = std::env::current_exe().expect("the test binary's path");
    let mut command = Command::new(exe);
    command
        .args([
            "cross_host_end",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("DC_CROSS_HOST_ROLE", role)
        .env("DC_CROSS_HOST_RESULTS", results)
        .envs(LOOPBACK_SCHEDULE);
    command
}

/// The server end's child process. Dropping it kills and reaps the server, so that a test that
/// fails after it starts the server, such as in the wait for its `listening` line, doesn't leave
/// the server running.
struct ServerEnd(Child);

impl Drop for ServerEnd {
    fn drop(&mut self) {
        // Where the test has reaped the server already, `kill` doesn't signal any process and
        // `wait` returns the status it reaped.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Dropping a `ServerEnd` kills the server end, which otherwise runs until a client ends its run:
/// the server's stdout reaches its end only once the server has exited.
#[test]
fn dropping_the_server_end_kills_it() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dc-cross-host-drop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the results directory");
    let mut server = ServerEnd(
        end_command("server", &dir.join("server.jsonl"))
            .env("DC_CROSS_HOST_LISTEN", "127.0.0.1:0")
            .stdout(Stdio::piped())
            .spawn()
            .expect("start the server end"),
    );
    let mut output = BufReader::new(server.0.stdout.take().expect("the server end's stdout"));
    listening_addr(&mut output);
    drop(server);
    io::copy(&mut output, &mut io::sink()).expect("the server end's output");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reads the server end's output up to its `listening` line, and returns the handshake address
/// that line states. Where the test harness runs one test at a time, it writes
/// `test cross_host_end ... ` before the test's output on the same line, so the server writes its
/// JSON line after a line break, and this skips each line that isn't the server's JSON object.
fn listening_addr(output: &mut impl BufRead) -> SocketAddr {
    let mut line = String::new();
    loop {
        line.clear();
        let len = output.read_line(&mut line).expect("the server end's output");
        assert!(len > 0, "the server end exited before it listened");
        let value: Value = match serde_json::from_str(line.trim()) {
            Ok(value) => value,
            // A line of the test harness's output.
            Err(_) => continue,
        };
        if value["event"] == "listening" {
            let addr = value["handshake_addr"].as_str().expect("handshake address");
            return addr.parse().expect("handshake address");
        }
    }
}

/// `listening_addr` finds the server's `listening` line after the test harness's output, both where
/// the harness runs tests in parallel and where it runs one test at a time, which leaves
/// `test cross_host_end ... ` at the start of the line that the server's output follows.
#[test]
fn listening_addr_after_the_harness_output() {
    let listening = json!({
        "event": "listening",
        "handshake_addr": "127.0.0.1:4433",
        "data_addrs": ["127.0.0.1:4434"],
    });
    let expected: SocketAddr = "127.0.0.1:4433".parse().unwrap();
    for harness in ["\nrunning 1 test\n", "\nrunning 1 test\ntest cross_host_end ... "] {
        // The server end prints its line as `run_server` does.
        let output = format!("{harness}\n{listening}\n");
        assert_eq!(listening_addr(&mut output.as_bytes()), expected, "{output:?}");
    }
}

/// The lines of `lines` whose event is `kind`.
fn events<'a>(lines: &'a [Value], kind: &str) -> Vec<&'a Value> {
    lines.iter().filter(|line| line["event"] == kind).collect()
}

fn read_lines(path: &Path) -> Vec<Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => panic!("{}: {error}", path.display()),
    };
    text.lines().map(parse_line).collect()
}

fn parse_line(line: &str) -> Value {
    match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => panic!("{line}: {error}"),
    }
}

/// The server end: takes the client's handshakes at `DC_CROSS_HOST_LISTEN`, writes each request
/// back to the client, and returns once the client's last stream ends the run.
async fn run_server(schedule: Schedule) {
    let listen: SocketAddr = parse("DC_CROSS_HOST_LISTEN");
    let results = Rc::new(RefCell::new(Results::open("server", schedule)));
    let (stream_endpoint, map) = new_endpoint(listen.ip()).expect("the stream endpoint");
    let tls = tls_server().expect("the handshake's TLS server");
    let handshake = psk::server::Provider::builder()
        .start(listen, tls, HandshakeEvents::default(), map)
        .await
        .expect("the handshake server");
    let handshake_addr = handshake.local_addr();
    let data_addrs: Vec<String> = stream_endpoint
        .data_addrs
        .iter()
        .map(SocketAddr::to_string)
        .collect();
    let server = stream::Server::new(stream_endpoint, handshake);
    let mut echo = server
        .register_acceptor(ECHO_ACCEPTOR, acceptor::channel::Config::default())
        .expect("the echo acceptor");
    let mut end = server
        .register_acceptor(END_ACCEPTOR, acceptor::channel::Config::default())
        .expect("the end acceptor");

    let listening = json!({
        "event": "listening",
        "handshake_addr": handshake_addr.to_string(),
        "data_addrs": data_addrs,
    });
    results.borrow_mut().write(listening.clone());
    // `loopback` reads the handshake address from this line on stdout, since its server listens on
    // port 0. The line break before it ends any line the test harness started, so that the JSON
    // object is a line of its own.
    println!("\n{listening}");

    let started = Instant::now();
    let mut echoes = tokio::task::JoinSet::new();
    let mut count = 0u64;
    let end_stream = loop {
        tokio::select! {
            stream = echo.recv() => {
                let stream = stream.expect("the echo acceptor closed");
                let results = results.clone();
                let index = count;
                echoes.spawn_local(async move {
                    let start = started.elapsed();
                    let result = echo_stream(stream).await;
                    let record = Record {
                        index,
                        start,
                        end: started.elapsed(),
                        result,
                    };
                    results.borrow_mut().write(record.to_json());
                });
                count += 1;
            }
            stream = end.recv() => break stream.expect("the end acceptor closed"),
        }
    };

    // The client waits for the end stream's response, so the server writes it back before it
    // waits for the streams that are still open.
    let end_result = echo_stream(end_stream).await;
    while echoes.join_next().await.is_some() {}
    let mut line = json!({ "event": "end", "streams": count });
    outcome(&mut line, &end_result);
    results.borrow_mut().write(line);
    end_result.expect("the end stream failed");
}

/// Reads a request to its end and writes it back with FIN; returns the request's length.
async fn echo_stream(stream: Stream) -> io::Result<usize> {
    let (mut reader, mut writer) = stream.into_split();
    let mut request = BytesMut::new();
    while reader.read_into(&mut request).await? != 0 {}
    let len = request.len();
    let mut response = request.freeze();
    writer.write_all_from_fin(&mut response).await?;
    Ok(len)
}

/// The client end: starts a stream each `DC_CROSS_HOST_STREAM_INTERVAL_MS` until
/// `DC_CROSS_HOST_RUN_MS` from `started`, ends the server's run, and asserts the outcome of its
/// streams under the schedule.
async fn run_client(schedule: Schedule, started: Instant) {
    let peer: SocketAddr = parse("DC_CROSS_HOST_PEER");
    let run = millis("DC_CROSS_HOST_RUN_MS");
    let interval = millis("DC_CROSS_HOST_STREAM_INTERVAL_MS");
    let size: usize = parse("DC_CROSS_HOST_STREAM_BYTES");
    assert!(
        !interval.is_zero(),
        "DC_CROSS_HOST_STREAM_INTERVAL_MS is zero"
    );
    assert!(size > 0, "DC_CROSS_HOST_STREAM_BYTES is zero");
    assert!(
        !schedule.has_drop() || run > schedule.drop_end(),
        "the run ends before the link drop does: DC_CROSS_HOST_RUN_MS has to outlast the drop's \
         end by the idle timeout ({} ms) and the dead-peer cooldown ({} ms), so that a stream \
         that starts after the drop ends completes and one more stream follows it",
        ms(endpoint::DEFAULT_IDLE_TIMEOUT),
        ms(endpoint::DEFAULT_DEAD_PEER_COOLDOWN)
    );
    let mut results = Results::open("client", schedule);

    // The data sockets and the handshake socket bind to the unspecified address of the peer's
    // family: the server takes the IP of the advertised data addresses from the handshake's source.
    let ip = match peer {
        SocketAddr::V4(_) => IpAddr::from(Ipv4Addr::UNSPECIFIED),
        SocketAddr::V6(_) => IpAddr::from(Ipv6Addr::UNSPECIFIED),
    };
    let (stream_endpoint, map) = new_endpoint(ip).expect("the stream endpoint");
    let handshake = psk::client::Provider::builder()
        .start(
            SocketAddr::new(ip, 0),
            map,
            tls_client().expect("the handshake's TLS client"),
            HandshakeEvents::default(),
            Name::from(SERVER_NAME),
        )
        .expect("the handshake client");
    let mut client = stream::Client::new(stream_endpoint, handshake, Name::from(SERVER_NAME));

    let mut records = Vec::new();
    while started.elapsed() < run {
        let index = records.len() as u64;
        let start = started.elapsed();
        let result = exchange(&mut client, peer, ECHO_ACCEPTOR, payload(index, size)).await;
        let record = Record {
            index,
            start,
            end: started.elapsed(),
            result,
        };
        let mut line = record.to_json();
        line["phase"] = schedule.phase(start).into();
        results.write(line);
        records.push(record);
        if let Some(wait) = (start + interval).checked_sub(started.elapsed()) {
            tokio::time::sleep(wait).await;
        }
    }

    if schedule.has_drop() {
        results.write(reconnection(&records, &schedule));
    }
    let end_request = Bytes::from_static(b"end");
    let end_result = exchange(&mut client, peer, END_ACCEPTOR, end_request).await;
    let mut line = json!({ "event": "end", "streams": records.len() });
    outcome(&mut line, &end_result);
    results.write(line);

    check(&records, &schedule);
    end_result.expect("the end stream failed");
}

/// Opens a stream to `acceptor`, writes `request` with FIN and reads the response to its end;
/// fails where the response differs from the request, which the server writes back.
async fn exchange(
    client: &mut stream::Client,
    peer: SocketAddr,
    acceptor: VarInt,
    request: Bytes,
) -> io::Result<usize> {
    let stream = client.connect(peer, acceptor, Priority::default()).await?;
    let (mut reader, mut writer) = stream.into_split();
    writer.write_all_from_fin(&mut request.clone()).await?;
    let mut response = BytesMut::with_capacity(request.len());
    while reader.read_into(&mut response).await? != 0 {}
    if response[..] != request[..] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the response's {} bytes differ from the request's {}",
                response.len(),
                request.len()
            ),
        ));
    }
    Ok(request.len())
}

/// The request of stream `index`: `size` bytes that differ from one stream to the next.
fn payload(index: u64, size: usize) -> Bytes {
    (0..size)
        .map(|offset| (index as usize).wrapping_add(offset) as u8)
        .collect()
}

/// Asserts that the streams completed under the scheduled loss, and that after the link drop the
/// client reconnected and the streams after the reconnection completed.
fn check(records: &[Record], schedule: &Schedule) {
    assert!(!records.is_empty(), "the client ran zero streams");
    for record in records {
        if !schedule.has_drop() || record.end <= schedule.drop_start {
            assert!(
                record.result.is_ok(),
                "stream {} failed under the scheduled loss: {:?}",
                record.index,
                record.result
            );
        }
    }
    if !schedule.has_drop() {
        return;
    }
    let reconnect = reconnect_stream(records, schedule)
        .expect("the client didn't complete a stream that started after the link drop ended");
    let after = &records[reconnect.index as usize + 1..];
    assert!(!after.is_empty(), "the run ended at the reconnection");
    for record in after {
        assert!(
            record.result.is_ok(),
            "stream {} failed after the reconnection: {:?}",
            record.index,
            record.result
        );
    }
}

/// The first stream that started after the link drop ended and completed: the reconnection.
fn reconnect_stream<'a>(records: &'a [Record], schedule: &Schedule) -> Option<&'a Record> {
    let drop_end = schedule.drop_end();
    records
        .iter()
        .find(|record| record.start >= drop_end && record.result.is_ok())
}

/// The reconnection's line: the stream that reconnected, when it ended, and how many streams
/// failed from the drop's start up to it.
fn reconnection(records: &[Record], schedule: &Schedule) -> Value {
    let drop_end = schedule.drop_end();
    let reconnect = reconnect_stream(records, schedule);
    let until = match reconnect {
        Some(record) => record.index as usize,
        None => records.len(),
    };
    let failed = records[..until]
        .iter()
        .filter(|record| record.end > schedule.drop_start && record.result.is_err())
        .count();
    json!({
        "event": "reconnect",
        "reconnected": reconnect.is_some(),
        "stream": reconnect.map(|record| record.index),
        "failed_streams": failed,
        "reconnected_ms": reconnect.map(|record| ms(record.end)),
        "after_drop_end_ms": reconnect.map(|record| ms(record.end - drop_end)),
    })
}

/// One stream's outcome, with its times from the start of the end that ran it.
struct Record {
    index: u64,
    start: Duration,
    end: Duration,
    result: io::Result<usize>,
}

impl Record {
    fn to_json(&self) -> Value {
        let mut line = json!({
            "event": "stream",
            "stream": self.index,
            "start_ms": ms(self.start),
            "end_ms": ms(self.end),
        });
        outcome(&mut line, &self.result);
        line
    }
}

/// Sets the outcome of a stream on its line: the bytes of its request, or its error.
fn outcome(line: &mut Value, result: &io::Result<usize>) {
    match result {
        Ok(bytes) => {
            line["outcome"] = "ok".into();
            line["bytes"] = (*bytes).into();
        }
        Err(error) => {
            line["outcome"] = "error".into();
            line["error_kind"] = format!("{:?}", error.kind()).into();
            line["error"] = error.to_string().into();
        }
    }
}

/// The fault schedule that the host applies between the two ends.
#[derive(Clone, Copy, Debug)]
struct Schedule {
    loss_percent: f64,
    drop_start: Duration,
    drop_len: Duration,
}

impl Schedule {
    fn from_env() -> Self {
        let loss_percent: f64 = parse("DC_CROSS_HOST_LOSS_PERCENT");
        assert!(
            (0.0..=100.0).contains(&loss_percent),
            "DC_CROSS_HOST_LOSS_PERCENT={loss_percent} isn't a percent"
        );
        Self {
            loss_percent,
            drop_start: millis("DC_CROSS_HOST_DROP_START_MS"),
            drop_len: millis("DC_CROSS_HOST_DROP_MS"),
        }
    }

    fn has_drop(&self) -> bool {
        !self.drop_len.is_zero()
    }

    fn drop_end(&self) -> Duration {
        self.drop_start + self.drop_len
    }

    /// The part of the schedule in which a stream that starts at `start` starts.
    fn phase(&self, start: Duration) -> &'static str {
        if start < self.drop_start {
            "before_drop"
        } else if start < self.drop_end() {
            "during_drop"
        } else {
            "after_drop"
        }
    }

    fn to_json(self) -> Value {
        json!({
            "loss_percent": self.loss_percent,
            "drop_start_ms": ms(self.drop_start),
            "drop_ms": ms(self.drop_len),
        })
    }
}

/// The end's results file, to which it appends one JSON line for each thing that happens.
struct Results {
    role: &'static str,
    schedule: Value,
    file: std::fs::File,
}

impl Results {
    fn open(role: &'static str, schedule: Schedule) -> Self {
        let path = var("DC_CROSS_HOST_RESULTS");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path);
        let file = match file {
            Ok(file) => file,
            Err(error) => panic!("{path}: {error}"),
        };
        Self {
            role,
            schedule: schedule.to_json(),
            file,
        }
    }

    /// Appends `line`, with the end's role and the schedule it ran under.
    fn write(&mut self, mut line: Value) {
        line["role"] = Value::from(self.role);
        line["schedule"] = self.schedule.clone();
        writeln!(self.file, "{line}").expect("the results file");
    }
}

/// One end's stream endpoint on the production socket configuration, with one socket and one
/// worker for each part of the pipeline. The data sockets bind to `ip` on an ephemeral port, and
/// the handshake advertises their addresses.
fn new_endpoint(ip: IpAddr) -> io::Result<(Arc<Endpoint>, Map)> {
    let gso = endpoint::Gso::default();
    let bind_addr = endpoint::socket::BindAddress::from(SocketAddr::new(ip, 0));
    let socket_config = endpoint::socket::Config::new(vec![bind_addr], 1, 1, gso.clone());
    let (send_sockets, recv_sockets) = socket_config.tokio()?;
    let data_addrs = recv_sockets
        .iter()
        .map(LocalAddr::local_addr)
        .collect::<io::Result<Vec<_>>>()?;

    // The map holds the addresses the handshake advertises, so it's built once the sockets are
    // bound, with the settings of the dc-tester load tester.
    let map = Map::builder()
        .with_signer(Signer::random())
        .with_capacity(50_000)
        .with_evict_on_unknown_path_secret(true)
        .with_advertised_data_addrs(&data_addrs)
        .with_clock(s2n_quic_dc::time::tokio::Clock::default())
        .with_subscriber(event::tracing::Subscriber::default())
        .build()
        .map_err(io::Error::other)?;

    // One socket for UnknownPathSecret responses, as the dc-tester load tester binds it.
    let ups = {
        let mut options = s2n_quic_dc::socket::Options::default();
        options.addr = SocketAddr::new(ip, 0);
        options.blocking = false;
        options.send_buffer = Some(1 << 20);
        options.recv_buffer = Some(0);
        let socket = options.build_udp()?;
        s2n_quic_dc::socket::Gso(s2n_quic_dc::socket::BusyPoll(socket), gso.clone())
    };

    // One worker for each part of the pipeline, in the order the crate's simulated endpoints
    // number them.
    let layout = WorkerLayout {
        frame_dispatch: 0,
        send: vec![1],
        recv_io: vec![2],
        recv_dispatch: vec![3],
        waker_drain: vec![4],
        background: 5,
    };
    let workers = runtime::tokio::Handle::new(6);
    // The values the crate's simulated endpoints run with, and the crate's defaults elsewhere.
    let config = endpoint::Config {
        counters: s2n_quic_dc::counter::Registry::default(),
        layout,
        recv_backend: RecvBackend::default(),
        send_pool: Pool::new(u16::MAX),
        recv_pool: Pool::new(u16::MAX),
        path_secret_map: map.clone(),
        gso,
        acceptor_registry: acceptor::Registry::new(),
        overall_send_rate: Rate::new(25.0),
        budgets: Budgets::default(),
        submission_shards: 4,
        ups_rate: Rate::new(0.001),
        ups_dedup_capacity: 1024,
        ups_dedup_window: Duration::from_secs(1),
        dead_peer_cooldown: endpoint::DEFAULT_DEAD_PEER_COOLDOWN,
        initial_tx_descriptor_allocs: 0,
        initial_rx_descriptor_allocs: 0,
        send_credit_pool_config: credit::Config::default(),
        recv_credit_pool_config: credit::Config::default(),
    };
    let inner = endpoint::setup_endpoint(workers, config, send_sockets, recv_sockets, ups);
    Ok((Arc::new(inner), map))
}

fn tls_server() -> io::Result<tls::Server> {
    tls::Server::builder()
        .with_application_protocols(["dcquic"].iter())
        .map_err(io::Error::other)?
        .with_certificate(certificates::CERT_PEM, certificates::KEY_PEM)
        .map_err(io::Error::other)?
        .build()
        .map_err(io::Error::other)
}

fn tls_client() -> io::Result<tls::Client> {
    tls::Client::builder()
        .with_application_protocols(["dcquic"].iter())
        .map_err(io::Error::other)?
        .with_certificate(certificates::CERT_PEM)
        .map_err(io::Error::other)?
        .build()
        .map_err(io::Error::other)
}

/// Writes the crate's logs to stderr, at the level `S2N_LOG` sets, and warnings where it isn't
/// set.
fn init_logs() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("S2N_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .try_init();
}

fn var(name: &str) -> String {
    match std::env::var(name) {
        Ok(value) => value,
        Err(error) => panic!("{name}: {error}"),
    }
}

fn parse<T>(name: &str) -> T
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let value = var(name);
    match value.parse() {
        Ok(parsed) => parsed,
        Err(error) => panic!("{name}={value}: {error}"),
    }
}

fn millis(name: &str) -> Duration {
    Duration::from_millis(parse(name))
}

fn ms(duration: Duration) -> u64 {
    duration.as_millis() as u64
}
