// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;

/// Runs a connection over a network whose packets report `receive_wait`, and returns the waits
/// the server's and the client's connections published
fn published_waits(receive_wait: Option<Duration>) -> (Vec<Duration>, Vec<Duration>) {
    let model = Model::default();
    model.set_receive_wait(receive_wait);

    let server_recorder = recorder::DatagramReceiveWait::new();
    let server_events = server_recorder.events();
    let client_recorder = recorder::DatagramReceiveWait::new();
    let client_events = client_recorder.events();

    test(model.clone(), |handle| {
        let server = Server::builder()
            .with_io(handle.builder().build().unwrap())?
            .with_tls(SERVER_CERTS)?
            .with_event((tracing_events(true, model.clone()), server_recorder))?
            .with_random(Random::with_seed(456))?
            .start()?;

        let client = Client::builder()
            .with_io(handle.builder().build().unwrap())?
            .with_tls(certificates::CERT_PEM)?
            .with_event((tracing_events(true, model.clone()), client_recorder))?
            .with_random(Random::with_seed(456))?
            .start()?;

        let addr = start_server(server)?;
        start_client(client, addr, Data::new(1000))?;
        Ok(addr)
    })
    .unwrap();

    let server_waits = server_events
        .lock()
        .unwrap()
        .iter()
        .map(|event| event.wait)
        .collect();
    let client_waits = client_events
        .lock()
        .unwrap()
        .iter()
        .map(|event| event.wait)
        .collect();

    (server_waits, client_waits)
}

/// Each datagram that holds a receive wait publishes that wait to the connection that reads it
#[test]
fn datagram_receive_wait_test() {
    let receive_wait = Duration::from_micros(1500);
    let (server_waits, client_waits) = published_waits(Some(receive_wait));

    assert!(!server_waits.is_empty());
    assert!(server_waits.iter().all(|wait| *wait == receive_wait));
    assert!(!client_waits.is_empty());
    assert!(client_waits.iter().all(|wait| *wait == receive_wait));
}

/// A datagram without a receive wait doesn't publish one
#[test]
fn no_datagram_receive_wait_test() {
    let (server_waits, client_waits) = published_waits(None);

    assert!(server_waits.is_empty());
    assert!(client_waits.is_empty());
}
