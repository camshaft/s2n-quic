// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;

/// What one side's connections published: the waits, and how many datagrams they received
struct Published {
    waits: Vec<Duration>,
    received: usize,
}

/// Runs a connection over a network whose packets report `receive_wait`, and returns what the
/// server's and the client's connections published
fn published(receive_wait: Option<Duration>) -> (Published, Published) {
    let model = Model::default();
    model.set_receive_wait(receive_wait);

    let server_recorder = recorder::DatagramReceiveWait::new();
    let server_events = server_recorder.events();
    let server_received = recorder::DatagramReceived::new();
    let server_received_events = server_received.events();
    let client_recorder = recorder::DatagramReceiveWait::new();
    let client_events = client_recorder.events();
    let client_received = recorder::DatagramReceived::new();
    let client_received_events = client_received.events();

    test(model.clone(), |handle| {
        let server = Server::builder()
            .with_io(handle.builder().build().unwrap())?
            .with_tls(SERVER_CERTS)?
            .with_event((
                tracing_events(true, model.clone()),
                (server_recorder, server_received),
            ))?
            .with_random(Random::with_seed(456))?
            .start()?;

        let client = Client::builder()
            .with_io(handle.builder().build().unwrap())?
            .with_tls(certificates::CERT_PEM)?
            .with_event((
                tracing_events(true, model.clone()),
                (client_recorder, client_received),
            ))?
            .with_random(Random::with_seed(456))?
            .start()?;

        let addr = start_server(server)?;
        start_client(client, addr, Data::new(1000))?;
        Ok(addr)
    })
    .unwrap();

    let server = Published {
        waits: server_events
            .lock()
            .unwrap()
            .iter()
            .map(|event| event.wait)
            .collect(),
        received: server_received_events.lock().unwrap().len(),
    };
    let client = Published {
        waits: client_events
            .lock()
            .unwrap()
            .iter()
            .map(|event| event.wait)
            .collect(),
        received: client_received_events.lock().unwrap().len(),
    };

    (server, client)
}

/// Each datagram a connection received publishes the wait it carried, the server's first
/// datagram among them
#[test]
fn datagram_receive_wait_test() {
    let receive_wait = Duration::from_micros(1500);
    let (server, client) = published(Some(receive_wait));

    assert!(server.received > 0);
    assert_eq!(server.waits.len(), server.received);
    assert!(server.waits.iter().all(|wait| *wait == receive_wait));
    assert!(client.received > 0);
    assert_eq!(client.waits.len(), client.received);
    assert!(client.waits.iter().all(|wait| *wait == receive_wait));
}

/// A datagram without a receive wait doesn't publish one
#[test]
fn no_datagram_receive_wait_test() {
    let (server, client) = published(None);

    assert!(server.received > 0);
    assert!(server.waits.is_empty());
    assert!(client.received > 0);
    assert!(client.waits.is_empty());
}
