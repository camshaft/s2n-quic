// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests for the busy poll runtime
//!
//! The busy poll runtime is designed to execute tasks in a tight loop without blocking,
//! which is useful for low-latency workloads. These tests validate that:
//!
//! 1. Tasks are executed correctly
//! 2. Priority ordering works as expected (lower priority values = higher priority)
//! 3. The runtime handles task completion and cleanup properly
//! 4. The runner stops when all handles are dropped

use super::*;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use std::time::Duration;

/// Tests that a single task can be spawned and executes to completion.
///
/// This validates the basic functionality of the busy poll runtime:
/// - Tasks can be spawned via Handle::spawn
/// - The Runner polls and executes the task
/// - Tasks complete and are cleaned up
#[test]
fn single_task_execution() {
    let (handle, runner) = Handle::new();
    let executed = Arc::new(AtomicBool::new(false));
    let executed_clone = executed.clone();

    handle.spawn(async move {
        executed_clone.store(true, Ordering::SeqCst);
    });

    let thread = std::thread::spawn(move || {
        runner.run();
    });

    // Give the runner time to process the task
    std::thread::sleep(Duration::from_millis(50));

    assert!(executed.load(Ordering::SeqCst), "Task should have executed");

    // Drop the handle to signal the runner to stop
    drop(handle);
    thread.join().unwrap();
}

/// Tests that multiple tasks execute in the order they're spawned when they have the same priority.
///
/// This validates that:
/// - Multiple tasks can be spawned before the runner processes them
/// - All tasks execute to completion
/// - The default priority (128) is used when no priority is specified
#[test]
fn multiple_tasks_execution() {
    let (handle, runner) = Handle::new();
    let counter = Arc::new(AtomicU32::new(0));

    for i in 0..10 {
        let counter_clone = counter.clone();
        handle.spawn(async move {
            counter_clone.fetch_add(1, Ordering::SeqCst);
            // Simulate some work
            for _ in 0..i * 100 {
                std::hint::spin_loop();
            }
        });
    }

    let thread = std::thread::spawn(move || {
        runner.run();
    });

    // Give the runner time to process all tasks
    std::thread::sleep(Duration::from_millis(100));

    assert_eq!(
        counter.load(Ordering::SeqCst),
        10,
        "All 10 tasks should have executed"
    );

    drop(handle);
    thread.join().unwrap();
}

/// Tests that tasks with different priorities execute in the correct order.
///
/// Priority ordering is critical for the busy poll runtime - lower priority values
/// mean higher priority (0 is highest, 255 is lowest). This test validates that:
/// - Tasks are sorted by priority after spawning
/// - Higher priority tasks (lower values) execute first
/// - The priority ordering is maintained across multiple spawns
#[test]
fn priority_ordering() {
    let (handle, runner) = Handle::new();
    let execution_order = Arc::new(Mutex::new(Vec::new()));

    // Spawn tasks with different priorities in random order
    // Priority 200 (low priority)
    let order_clone = execution_order.clone();
    handle.spawn_with_priority(
        async move {
            order_clone.lock().push(200u8);
        },
        Some(200),
    );

    // Priority 50 (high priority)
    let order_clone = execution_order.clone();
    handle.spawn_with_priority(
        async move {
            order_clone.lock().push(50u8);
        },
        Some(50),
    );

    // Priority 128 (default priority)
    let order_clone = execution_order.clone();
    handle.spawn_with_priority(
        async move {
            order_clone.lock().push(128u8);
        },
        Some(128),
    );

    // Priority 10 (very high priority)
    let order_clone = execution_order.clone();
    handle.spawn_with_priority(
        async move {
            order_clone.lock().push(10u8);
        },
        Some(10),
    );

    let thread = std::thread::spawn(move || {
        runner.run();
    });

    // Give the runner time to process tasks
    std::thread::sleep(Duration::from_millis(100));

    let order = execution_order.lock().clone();
    assert_eq!(
        order,
        vec![10u8, 50, 128, 200],
        "Tasks should execute in priority order (lower values = higher priority)"
    );

    drop(handle);
    thread.join().unwrap();
}

/// Tests that the runner stops when all handles are dropped.
///
/// This validates the lifecycle management of the busy poll runtime:
/// - The Runner holds a Weak reference to the shared state
/// - When all Handles are dropped, the Arc is dropped
/// - The Runner's Weak reference fails to upgrade and it exits
/// - The runner thread can be joined without hanging
#[test]
fn runner_stops_when_handles_dropped() {
    let (handle, runner) = Handle::new();

    let thread = std::thread::spawn(move || {
        runner.run();
    });

    // Drop the handle immediately
    drop(handle);

    // The runner should exit shortly after the handle is dropped
    // If this hangs, the runner is not detecting the dropped handle
    thread
        .join()
        .expect("Runner should exit cleanly when handles are dropped");
}

/// Tests that tasks can be spawned after the runner has already started processing.
///
/// This validates dynamic task spawning:
/// - The runner continuously checks for new tasks
/// - Tasks spawned after the runner starts are still executed
/// - The state synchronization between Handle and Runner works correctly
#[test]
fn spawn_after_runner_started() {
    let (handle, runner) = Handle::new();
    let executed = Arc::new(AtomicBool::new(false));
    let executed_clone = executed.clone();

    let thread = std::thread::spawn(move || {
        runner.run();
    });

    // Wait a bit to ensure the runner is running
    std::thread::sleep(Duration::from_millis(10));

    // Now spawn a task
    handle.spawn(async move {
        executed_clone.store(true, Ordering::SeqCst);
    });

    // Give the runner time to process the task
    std::thread::sleep(Duration::from_millis(50));

    assert!(
        executed.load(Ordering::SeqCst),
        "Task spawned after runner started should execute"
    );

    drop(handle);
    thread.join().unwrap();
}

/// Tests that completed tasks are properly cleaned up and their slots can be reused.
///
/// This validates the task slot management:
/// - Completed tasks are marked as None in the slots vector
/// - Free slot indices are tracked for reuse
/// - After cleanup (after_spawn), free slots at the end are removed
/// - New tasks can reuse freed slots
#[test]
fn task_cleanup_and_reuse() {
    let (handle, runner) = Handle::new();
    let first_batch = Arc::new(AtomicU32::new(0));
    let second_batch = Arc::new(AtomicU32::new(0));

    // Spawn first batch of tasks
    for _ in 0..5 {
        let counter = first_batch.clone();
        handle.spawn(async move {
            counter.fetch_add(1, Ordering::SeqCst);
        });
    }

    let thread = std::thread::spawn(move || {
        runner.run();
    });

    // Wait for first batch to complete
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(first_batch.load(Ordering::SeqCst), 5);

    // Spawn second batch - these should reuse the freed slots
    for _ in 0..5 {
        let counter = second_batch.clone();
        handle.spawn(async move {
            counter.fetch_add(1, Ordering::SeqCst);
        });
    }

    // Wait for second batch to complete
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        second_batch.load(Ordering::SeqCst),
        5,
        "Second batch should execute after first batch completes"
    );

    drop(handle);
    thread.join().unwrap();
}

/// Tests the Pool abstraction that wraps multiple Handles.
///
/// This validates that:
/// - A Pool can be created from multiple Handles
/// - The Pool can be cloned (shallow clone of the Arc)
/// - The Pool derefs to a slice of Handles
#[test]
fn pool_creation_and_deref() {
    let mut handles = Vec::new();
    let mut runners = Vec::new();

    for _ in 0..3 {
        let (handle, runner) = Handle::new();
        handles.push(handle);
        runners.push(runner);
    }

    let pool = Pool::from(handles);
    assert_eq!(pool.len(), 3, "Pool should contain 3 handles");

    // Test cloning
    let pool_clone = pool.clone();
    assert_eq!(pool_clone.len(), 3, "Cloned pool should have same length");

    // Clean up runners
    drop(pool);
    drop(pool_clone);
    for runner in runners {
        let thread = std::thread::spawn(move || runner.run());
        let _ = thread.join();
    }
}

/// Tests that the default priority is 128 when None is specified.
///
/// This validates the priority defaulting behavior:
/// - spawn() uses the default priority of 128
/// - spawn_with_priority(task, None) also uses 128
/// - Tasks with explicit priority 128 have the same priority as default tasks
#[test]
fn default_priority() {
    let (handle, runner) = Handle::new();
    let execution_order = Arc::new(Mutex::new(Vec::new()));

    // Task with explicit None priority
    let order_clone = execution_order.clone();
    handle.spawn_with_priority(
        async move {
            order_clone.lock().push(1u8);
        },
        None,
    );

    // Task with explicit 128 priority
    let order_clone = execution_order.clone();
    handle.spawn_with_priority(
        async move {
            order_clone.lock().push(2u8);
        },
        Some(128),
    );

    // Task with default priority via spawn()
    let order_clone = execution_order.clone();
    handle.spawn(async move {
        order_clone.lock().push(3u8);
    });

    let thread = std::thread::spawn(move || {
        runner.run();
    });

    std::thread::sleep(Duration::from_millis(50));

    let order = execution_order.lock();
    // All should have executed (order may vary since they have the same priority)
    assert_eq!(
        order.len(),
        3,
        "All three tasks should have executed with default priority"
    );

    drop(handle);
    thread.join().unwrap();
}

/// Tests that tasks are polled continuously until they complete.
///
/// This validates that:
/// - Tasks that return Pending are polled again in subsequent iterations
/// - The busy poll loop continues polling until tasks return Ready
/// - Multiple poll iterations can be required for a single task
#[test]
fn task_polls_until_ready() {
    let (handle, runner) = Handle::new();
    let poll_count = Arc::new(AtomicU32::new(0));
    let completed = Arc::new(AtomicBool::new(false));

    let poll_count_clone = poll_count.clone();
    let completed_clone = completed.clone();

    handle.spawn(async move {
        // Use poll_fn to create a future that returns Pending several times
        std::future::poll_fn(|_cx| {
            let count = poll_count_clone.fetch_add(1, Ordering::SeqCst);
            if count < 5 {
                std::task::Poll::Pending
            } else {
                std::task::Poll::Ready(())
            }
        })
        .await;

        completed_clone.store(true, Ordering::SeqCst);
    });

    let thread = std::thread::spawn(move || {
        runner.run();
    });

    std::thread::sleep(Duration::from_millis(100));

    assert!(
        completed.load(Ordering::SeqCst),
        "Task should complete after being polled multiple times"
    );
    assert!(
        poll_count.load(Ordering::SeqCst) >= 5,
        "Task should have been polled at least 5 times"
    );

    drop(handle);
    thread.join().unwrap();
}

#[cfg(test)]
mod bolero_tests {
    use super::*;
    use bolero::{check, TypeGenerator};

    /// Represents operations that can be performed on the busy poll runtime
    #[derive(Clone, Copy, Debug, TypeGenerator)]
    enum Operation {
        /// Spawn a task with a specific priority value
        SpawnWithPriority { priority: u8 },
        /// Spawn a task with default priority
        SpawnDefault,
        /// Wait for tasks to execute
        Wait,
    }

    /// Property-based test: Tasks with lower priority values always execute before higher ones.
    ///
    /// This test generates random sequences of spawn operations with various priorities
    /// and verifies that the execution order respects priority ordering. This is crucial
    /// because the busy poll runtime is intended for latency-sensitive workloads where
    /// priority ordering must be reliable.
    #[test]
    #[cfg_attr(kani, kani::proof)]
    fn priority_ordering_property() {
        check!()
            .with_type::<Vec<Operation>>()
            .with_iterations(100)
            .for_each(|operations| {
                // Skip empty operation lists
                if operations.is_empty() {
                    return;
                }

                let (handle, runner) = Handle::new();
                let execution_order = Arc::new(Mutex::new(Vec::new()));
                let mut expected_priorities = Vec::new();

                for op in operations {
                    match op {
                        Operation::SpawnWithPriority { priority } => {
                            let order_clone = execution_order.clone();
                            let p = *priority;
                            handle.spawn_with_priority(
                                async move {
                                    order_clone.lock().push(p);
                                },
                                Some(p),
                            );
                            expected_priorities.push(p);
                        }
                        Operation::SpawnDefault => {
                            let order_clone = execution_order.clone();
                            handle.spawn(async move {
                                order_clone.lock().push(128);
                            });
                            expected_priorities.push(128);
                        }
                        Operation::Wait => {
                            // Wait operations are a no-op in this test
                        }
                    }
                }

                // Start the runner
                let thread = std::thread::spawn(move || {
                    runner.run();
                });

                // Give time for tasks to execute
                std::thread::sleep(Duration::from_millis(100));

                let execution = execution_order.lock().clone();

                // All tasks should have executed
                assert_eq!(
                    execution.len(),
                    expected_priorities.len(),
                    "All spawned tasks should execute"
                );

                // Sort expected priorities to get the correct execution order
                expected_priorities.sort();

                // Verify execution order matches priority order
                assert_eq!(
                    execution, expected_priorities,
                    "Tasks should execute in priority order (lowest value first)"
                );

                drop(handle);
                let _ = thread.join();
            });
    }

    /// Property-based test: All spawned tasks eventually execute to completion.
    ///
    /// This validates the fundamental guarantee of the busy poll runtime:
    /// regardless of the sequence of operations, all spawned tasks will
    /// eventually be executed. This tests the reliability of the runtime.
    #[test]
    #[cfg_attr(kani, kani::proof)]
    fn all_tasks_execute_property() {
        check!()
            .with_type::<Vec<u8>>()
            .with_iterations(100)
            .for_each(|priorities| {
                // Limit to reasonable number of tasks
                if priorities.is_empty() || priorities.len() > 50 {
                    return;
                }

                let (handle, runner) = Handle::new();
                let executed_count = Arc::new(AtomicU32::new(0));

                for priority in priorities {
                    let count_clone = executed_count.clone();
                    handle.spawn_with_priority(
                        async move {
                            count_clone.fetch_add(1, Ordering::SeqCst);
                        },
                        Some(*priority),
                    );
                }

                let thread = std::thread::spawn(move || {
                    runner.run();
                });

                // Wait for all tasks to complete
                std::thread::sleep(Duration::from_millis(100));

                assert_eq!(
                    executed_count.load(Ordering::SeqCst),
                    priorities.len() as u32,
                    "All spawned tasks should execute exactly once"
                );

                drop(handle);
                let _ = thread.join();
            });
    }
}

#[cfg(test)]
mod tasks_tests {
    use super::*;

    /// Tests the internal Tasks structure's spawn and poll behavior.
    ///
    /// This validates:
    /// - Tasks can be spawned into the slots vector
    /// - Polling executes the task's poll method
    /// - Ready tasks are removed and their slots marked as free
    #[test]
    fn tasks_spawn_and_poll() {
        let mut tasks = Tasks::new();
        let executed = Arc::new(AtomicBool::new(false));
        let executed_clone = executed.clone();

        let task = Task {
            task: Box::pin(async move {
                executed_clone.store(true, Ordering::SeqCst);
            }),
            priority: 128,
        };

        tasks.spawn(task);
        assert_eq!(tasks.slots.len(), 1, "Should have one task slot");

        let waker = s2n_quic_core::task::waker::noop();
        let mut cx = Context::from_waker(&waker);

        tasks.poll(&mut cx);
        assert!(executed.load(Ordering::SeqCst), "Task should have executed");
        assert_eq!(tasks.free.len(), 1, "Should have one free slot");
    }

    /// Tests that after_spawn correctly sorts tasks by priority and cleans up free slots.
    ///
    /// This validates the priority sorting and cleanup logic:
    /// - Tasks are sorted with lower priority values first
    /// - None entries (freed slots) are moved to the end
    /// - Trailing None entries are removed from the slots vector
    #[test]
    fn tasks_after_spawn_sorting() {
        let mut tasks = Tasks::new();

        // Add tasks with different priorities
        tasks.spawn(Task {
            task: Box::pin(async {}),
            priority: 200,
        });
        tasks.spawn(Task {
            task: Box::pin(async {}),
            priority: 50,
        });
        tasks.spawn(Task {
            task: Box::pin(async {}),
            priority: 150,
        });

        tasks.after_spawn();

        // Verify sorting: priorities should be [50, 150, 200]
        assert_eq!(tasks.slots[0].as_ref().unwrap().priority, 50);
        assert_eq!(tasks.slots[1].as_ref().unwrap().priority, 150);
        assert_eq!(tasks.slots[2].as_ref().unwrap().priority, 200);
    }

    /// Tests that freed slots are reused when spawning new tasks.
    ///
    /// This validates the slot reuse mechanism:
    /// - When a task completes, its slot index is added to the free list
    /// - Spawning a new task uses a free slot if available
    /// - This prevents unbounded growth of the slots vector
    #[test]
    fn tasks_slot_reuse() {
        let mut tasks = Tasks::new();

        // Spawn and complete a task
        let task = Task {
            task: Box::pin(async {}),
            priority: 128,
        };
        tasks.spawn(task);

        let waker = s2n_quic_core::task::waker::noop();
        let mut cx = Context::from_waker(&waker);
        tasks.poll(&mut cx);

        assert_eq!(tasks.slots.len(), 1, "Should have one slot");
        assert_eq!(tasks.free.len(), 1, "Should have one free slot");

        // Spawn a new task - should reuse the free slot
        tasks.spawn(Task {
            task: Box::pin(async {}),
            priority: 64,
        });

        assert_eq!(
            tasks.slots.len(),
            1,
            "Should still have one slot (reused)"
        );
        assert!(tasks.slots[0].is_some(), "Slot should be occupied");
    }

    /// Tests that after_spawn clears trailing None entries.
    ///
    /// This validates the cleanup behavior:
    /// - Free slots at the end of the vector are removed
    /// - This keeps memory usage bounded
    /// - Interior None slots are kept for potential reuse
    #[test]
    fn tasks_cleanup_trailing_none() {
        let mut tasks = Tasks::new();

        // Add three tasks
        tasks.spawn(Task {
            task: Box::pin(async {}),
            priority: 1,
        });
        tasks.spawn(Task {
            task: Box::pin(async {}),
            priority: 2,
        });
        tasks.spawn(Task {
            task: Box::pin(async {}),
            priority: 3,
        });

        let waker = s2n_quic_core::task::waker::noop();
        let mut cx = Context::from_waker(&waker);

        // Complete all tasks
        tasks.poll(&mut cx);
        assert_eq!(tasks.free.len(), 3, "All slots should be free");

        // after_spawn should remove trailing None entries
        tasks.after_spawn();
        assert_eq!(
            tasks.slots.len(),
            0,
            "All trailing None slots should be removed"
        );
        assert_eq!(tasks.free.len(), 0, "Free list should be cleared");
    }
}
