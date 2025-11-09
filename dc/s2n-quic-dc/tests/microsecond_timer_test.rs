// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration test for microsecond-accurate timer

use s2n_quic_dc::clock::{tokio::Clock, Timer};
use std::time::Duration;
use s2n_quic_core::time::{clock::Timer as _, Clock as _};

#[tokio::test]
async fn test_microsecond_timer_basic() {
    let clock = Clock::default();
    let mut timer = Timer::new(&clock);
    
    // Test basic functionality
    let start = clock.get_time();
    timer.update(start + Duration::from_micros(100));
    timer.ready().await;
    let elapsed = clock.get_time() - start;
    
    // Should be close to 100 microseconds (allow some tolerance for scheduling)
    let elapsed_micros = elapsed.as_micros();
    println!("Elapsed: {} microseconds", elapsed_micros);
    assert!(elapsed_micros >= 100, "elapsed {} < 100 microseconds", elapsed_micros);
    // Allow up to 10ms for scheduling delays
    assert!(elapsed_micros < 10_000, "elapsed {} >= 10ms (too much delay)", elapsed_micros);
}

#[tokio::test]
async fn test_microsecond_timer_short_delay() {
    let clock = Clock::default();
    let mut timer = Timer::new(&clock);
    
    // Test with very short delay (500 microseconds)
    let start = clock.get_time();
    timer.update(start + Duration::from_micros(500));
    timer.ready().await;
    let elapsed = clock.get_time() - start;
    
    let elapsed_micros = elapsed.as_micros();
    println!("Elapsed: {} microseconds", elapsed_micros);
    assert!(elapsed_micros >= 500, "elapsed {} < 500 microseconds", elapsed_micros);
    assert!(elapsed_micros < 10_000, "elapsed {} >= 10ms (too much delay)", elapsed_micros);
}

#[tokio::test]
async fn test_microsecond_timer_long_delay() {
    let clock = Clock::default();
    let mut timer = Timer::new(&clock);
    
    // Test with longer delay (10ms) to verify we don't spin the whole time
    let start = clock.get_time();
    timer.update(start + Duration::from_millis(10));
    timer.ready().await;
    let elapsed = clock.get_time() - start;
    
    let elapsed_millis = elapsed.as_millis();
    println!("Elapsed: {} milliseconds", elapsed_millis);
    assert!(elapsed_millis >= 10, "elapsed {} < 10ms", elapsed_millis);
    assert!(elapsed_millis < 100, "elapsed {} >= 100ms (too much delay)", elapsed_millis);
}
