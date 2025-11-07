// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    stream::testing::{Client, Server},
    testing::{ext::*, sim, spawn},
};
use checkers::Machine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn memory_leaks() {
    sim(|| {
        async move {
            let client = Client::builder().build();

            for idx in 0..10 {
                // wipe the allocation snapshot
                checkers::with_state(|state| state.borrow_mut().clear());
                let unmute = checkers::mute_guard(false);

                {
                    let mut stream = client.connect_sim("server:443").await.unwrap();

                    let request = vec![42; 1_000];
                    stream.write_all(&request).await.unwrap();
                    stream.shutdown().await.unwrap();

                    let mut response = vec![];
                    stream.read_to_end(&mut response).await.unwrap();

                    assert_eq!(request, response);

                    10.s().sleep().await;
                    drop(stream);
                    10.s().sleep().await;
                }

                // don't capture the first one to avoid including global buffer resizes
                if idx <= 2 {
                    continue;
                }
                drop(unmute);

                let _muted = checkers::mute_guard(true);

                let machine = checkers::with_state(|state| {
                    let state = state.borrow();
                    let mut machine = Machine::default();
                    for event in state.events.iter() {
                        let _ = machine.push(event);
                    }
                    machine
                });

                let trailing_regions = machine.trailing_regions();
                if trailing_regions.is_empty() {
                    continue;
                }

                for region in &trailing_regions {
                    println!("\n==============\nleak detected {}", region.region);
                    if let Some(bt) = &region.backtrace {
                        println!("{bt:?}");
                    }
                }
                panic!("{} leaks detected", trailing_regions.len());
            }
        }
        .group("client")
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((mut stream, _addr)) = server.accept().await {
                spawn(async move {
                    let mut request = vec![];
                    stream.read_to_end(&mut request).await.unwrap();

                    stream.write_all(&request).await.unwrap();
                });
            }
        }
        .group("server")
        .spawn();
    });
}
