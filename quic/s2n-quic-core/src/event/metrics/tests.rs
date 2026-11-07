// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::aggregate::{self, info, AsVariant, Info};
use crate::event::{
    self, api, builder, ConnectionPublisher as _, ConnectionPublisherSubscriber, IntoEvent as _,
};
use core::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use std::sync::{Arc, Mutex};

fn meta() -> builder::ConnectionMeta {
    builder::ConnectionMeta {
        endpoint_type: crate::endpoint::Type::Server,
        id: 1,
        // SAFETY: the tests don't compare the timestamp with a clock's time
        timestamp: unsafe { crate::time::Timestamp::from_duration(Duration::from_secs(1)) },
    }
}

/// Creates a connection context of `subscriber`, and publishes a `DatagramReceiveWait` event to it
/// for each of `waits`
fn publish<S: event::Subscriber>(subscriber: &mut S, waits: &[Duration]) -> S::ConnectionContext {
    let mut context = subscriber.create_connection_context(
        &meta().into_event(),
        &builder::ConnectionInfo { application: None }.into_event(),
    );

    for wait in waits {
        let mut publisher = ConnectionPublisherSubscriber::new(meta(), 1, subscriber, &mut context);
        publisher.on_datagram_receive_wait(builder::DatagramReceiveWait { wait: *wait });
    }

    context
}

/// A registry whose every recorder sums the values it records
struct Sums;

impl aggregate::Registry for Sums {
    type Counter = AtomicU64;
    type BoolCounter = AtomicU64;
    type NominalCounter = AtomicU64;
    type Measure = AtomicU64;
    type Gauge = AtomicU64;
    type Timer = AtomicU64;
    type NominalTimer = AtomicU64;

    fn register_counter(&self, _info: &'static Info) -> Self::Counter {
        AtomicU64::new(0)
    }

    fn register_bool_counter(&self, _info: &'static Info) -> Self::BoolCounter {
        AtomicU64::new(0)
    }

    fn register_nominal_counter(
        &self,
        _info: &'static Info,
        _variant: &'static info::Variant,
    ) -> Self::NominalCounter {
        AtomicU64::new(0)
    }

    fn register_measure(&self, _info: &'static Info) -> Self::Measure {
        AtomicU64::new(0)
    }

    fn register_gauge(&self, _info: &'static Info) -> Self::Gauge {
        AtomicU64::new(0)
    }

    fn register_timer(&self, _info: &'static Info) -> Self::Timer {
        AtomicU64::new(0)
    }

    fn register_nominal_timer(
        &self,
        _info: &'static Info,
        _variant: &'static info::Variant,
    ) -> Self::NominalTimer {
        AtomicU64::new(0)
    }
}

/// The aggregate subscriber counts each `DatagramReceiveWait` event in its `datagram_receive_wait`
/// counter alone, since the event doesn't declare a measure or a timer
#[test]
fn aggregate_counts_datagram_receive_wait() {
    let mut subscriber = aggregate::Subscriber::new(Sums);
    let waits = [Duration::from_micros(5), Duration::from_micros(7)];
    let _context = publish(&mut subscriber, &waits);

    let mut found = false;
    for (info, counter) in subscriber.counters() {
        let name: &str = info.name;
        let value = counter.load(Ordering::Relaxed);
        if name == "datagram_receive_wait" {
            assert!(!found, "only one counter is named {name}");
            found = true;
            assert_eq!(value, waits.len() as u64);
        } else {
            assert_eq!(value, 0, "{name} counted a DatagramReceiveWait event");
        }
    }
    assert!(found, "the datagram_receive_wait counter is registered");

    for (info, measure) in subscriber.measures() {
        let name: &str = info.name;
        assert!(
            !name.starts_with("datagram_receive_wait"),
            "{name} is registered"
        );
        assert_eq!(
            measure.load(Ordering::Relaxed),
            0,
            "{name} measured a DatagramReceiveWait event"
        );
    }

    for (info, timer) in subscriber.timers() {
        let name: &str = info.name;
        assert!(
            !name.starts_with("datagram_receive_wait"),
            "{name} is registered"
        );
        assert_eq!(
            timer.load(Ordering::Relaxed),
            0,
            "{name} timed a DatagramReceiveWait event"
        );
    }

    for (info, counter) in subscriber.bool_counters() {
        let name: &str = info.name;
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "{name} counted a DatagramReceiveWait event"
        );
    }

    for (info, counters, _variants) in subscriber.nominal_counters() {
        let name: &str = info.name;
        for counter in counters {
            assert_eq!(
                counter.load(Ordering::Relaxed),
                0,
                "{name} counted a DatagramReceiveWait event"
            );
        }
    }
}

/// The probe registry registers a recorder for the `datagram_receive_wait` counter: it panics
/// while it registers a metric that doesn't have one
#[test]
fn probe_registers_datagram_receive_wait() {
    let mut subscriber = aggregate::Subscriber::new(aggregate::probe::Registry::default());
    let _context = publish(&mut subscriber, &[Duration::from_micros(5)]);
}

/// The counts a `Reported` recorder was given, by name
#[derive(Clone, Default)]
struct Reported(Arc<Mutex<Vec<(String, usize)>>>);

impl super::Recorder for Reported {
    fn increment_counter(&self, name: &str, amount: usize) {
        self.0.lock().unwrap().push((name.to_string(), amount));
    }

    fn set_value<V: core::fmt::Display>(&self, key: &str, value: V) {
        let _ = (key, value);
    }
}

/// A subscriber whose connection context is the `Reported` recorder it holds
struct Reporter(Reported);

impl event::Subscriber for Reporter {
    type ConnectionContext = Reported;

    fn create_connection_context(
        &mut self,
        _meta: &api::ConnectionMeta,
        _info: &api::ConnectionInfo,
    ) -> Self::ConnectionContext {
        self.0.clone()
    }
}

/// The metrics subscriber's connection context reports the count of `DatagramReceiveWait` events
/// in its `datagram_receive_wait` counter when it drops
#[test]
fn metrics_subscriber_reports_datagram_receive_wait() {
    let reported = Reported::default();
    let mut subscriber = super::Subscriber::new(Reporter(reported.clone()));
    let waits = [Duration::from_micros(5), Duration::from_micros(7)];
    let context = publish(&mut subscriber, &waits);

    assert!(reported.0.lock().unwrap().is_empty());
    drop(context);

    let counts = reported.0.lock().unwrap();
    let mut found = false;
    for (name, amount) in counts.iter() {
        if name == "datagram_receive_wait" {
            assert!(!found, "the context reports {name} once");
            found = true;
            assert_eq!(*amount, waits.len());
        } else {
            assert_eq!(*amount, 0, "{name} counted a DatagramReceiveWait event");
        }
    }
    assert!(found, "the context reports datagram_receive_wait");
}

/// Each `PlatformFeatureConfiguration` variant answers its index and its name, the
/// `RxTimestamps` variant among them
#[test]
fn platform_feature_configuration_variants() {
    use api::PlatformFeatureConfiguration as Configuration;

    let cases = [
        (Configuration::Gso { max_segments: 1 }, "GSO"),
        (Configuration::Gro { enabled: true }, "GRO"),
        (Configuration::Ecn { enabled: true }, "ECN"),
        (Configuration::BaseMtu { mtu: 1200 }, "BASE_MTU"),
        (Configuration::InitialMtu { mtu: 1200 }, "INITIAL_MTU"),
        (Configuration::MaxMtu { mtu: 1500 }, "MAX_MTU"),
        (Configuration::RxTimestamps { enabled: true }, "RX_TIMESTAMPS"),
    ];

    assert_eq!(<Configuration as AsVariant>::VARIANTS.len(), cases.len());

    for (idx, (configuration, name)) in cases.iter().enumerate() {
        assert_eq!(configuration.variant_idx(), idx);
        let variant = configuration.as_variant();
        assert_eq!(variant.id, idx);
        let variant_name: &str = variant.name;
        assert_eq!(variant_name, *name);
    }
}

/// The builder's `RxTimestamps` configuration becomes the event's, with the same `enabled`
#[test]
fn rx_timestamps_configuration_into_event() {
    for enabled in [false, true] {
        let configuration: api::PlatformFeatureConfiguration =
            builder::PlatformFeatureConfiguration::RxTimestamps { enabled }.into_event();
        assert!(matches!(
            configuration,
            api::PlatformFeatureConfiguration::RxTimestamps { enabled: value } if value == enabled
        ));
    }
}
