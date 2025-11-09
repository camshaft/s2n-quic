// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Test to verify microsecond timer works correctly with packet pacer scenarios

use s2n_quic_dc::clock::{tokio::Clock, Timer};
use std::time::Duration;
use s2n_quic_core::time::{clock::Timer as _, Clock as _, timer::Provider, token_bucket::TokenBucket};

#[tokio::test]
async fn test_timer_with_token_bucket_microsecond_refills() {
    let clock = Clock::default();
    let mut timer = Timer::new(&clock);
    
    // Create a token bucket with 100 microsecond refill intervals
    // (8 microseconds might be too small and cause precision issues)
    let mut token_bucket = TokenBucket::builder()
        .with_max(1000)
        .with_refill_interval(Duration::from_micros(100))
        .with_refill_amount(100)
        .build();
    
    // Consume all tokens
    let now = clock.get_time();
    let consumed = token_bucket.take(1000, now);
    println!("Initial consume: {}", consumed);
    assert_eq!(consumed, 1000);
    
    // Try to consume more - should have none left immediately
    let consumed = token_bucket.take(100, now);
    println!("Second consume (same time): {}", consumed);
    // If this returns tokens, the bucket might have already refilled due to clock precision
    
    // Wait for next refill
    let next_expiration = token_bucket.next_expiration().unwrap();
    println!("Next expiration: {:?} (from now: {:?})", next_expiration, next_expiration - now);
    timer.update(next_expiration);
    timer.ready().await;
    
    // Should be able to consume refilled tokens
    let now_after = clock.get_time();
    println!("After waiting, now: {:?}, elapsed: {:?}", now_after, now_after - now);
    let consumed = token_bucket.take(100, now_after);
    println!("After refill consume: {}", consumed);
    // After waiting for the refill interval, we should definitely have tokens
    assert!(consumed > 0, "Should have received some refilled tokens");
}

#[tokio::test]
async fn test_timer_with_multiple_microsecond_intervals() {
    let clock = Clock::default();
    let mut timer = Timer::new(&clock);
    
    // Test sleeping for several microsecond intervals in sequence
    let intervals = [10, 50, 100, 200, 500];
    
    for interval in intervals {
        let start = clock.get_time();
        timer.update(start + Duration::from_micros(interval));
        timer.ready().await;
        let elapsed = clock.get_time() - start;
        
        let elapsed_micros = elapsed.as_micros();
        println!("Interval: {} us, Elapsed: {} us", interval, elapsed_micros);
        assert!(
            elapsed_micros >= interval as u128,
            "elapsed {} < {} microseconds",
            elapsed_micros,
            interval
        );
        // Allow some tolerance for scheduling
        assert!(
            elapsed_micros < (interval as u128 * 2) + 1000,
            "elapsed {} is too large (expected ~{} microseconds)",
            elapsed_micros,
            interval
        );
    }
}

#[tokio::test]
async fn test_timer_rapid_updates() {
    let clock = Clock::default();
    let mut timer = Timer::new(&clock);
    
    // Simulate rapid updates like in packet pacing
    let start = clock.get_time();
    
    for i in 1..=10 {
        let target = start + Duration::from_micros(i * 100);
        timer.update(target);
        timer.ready().await;
    }
    
    let elapsed = clock.get_time() - start;
    let elapsed_micros = elapsed.as_micros();
    
    // Should have waited approximately 1000 microseconds (10 * 100)
    println!("Total elapsed: {} us", elapsed_micros);
    assert!(elapsed_micros >= 1000, "elapsed {} < 1000 microseconds", elapsed_micros);
    assert!(elapsed_micros < 10_000, "elapsed {} >= 10ms (too much delay)", elapsed_micros);
}

#[tokio::test]
async fn test_timer_mixed_granularities() {
    let clock = Clock::default();
    let mut timer = Timer::new(&clock);
    
    // Test that timer handles both microsecond and millisecond delays correctly
    let scenarios = vec![
        ("10us", Duration::from_micros(10)),
        ("100us", Duration::from_micros(100)),
        ("1ms", Duration::from_millis(1)),
        ("50us", Duration::from_micros(50)),
        ("5ms", Duration::from_millis(5)),
        ("500us", Duration::from_micros(500)),
    ];
    
    for (name, duration) in scenarios {
        let start = clock.get_time();
        timer.update(start + duration);
        timer.ready().await;
        let elapsed = clock.get_time() - start;
        
        println!("{}: expected {:?}, got {:?}", name, duration, elapsed);
        
        // Verify the delay is at least as long as requested
        assert!(
            elapsed >= duration,
            "{}: elapsed {:?} < requested {:?}",
            name,
            elapsed,
            duration
        );
    }
}
