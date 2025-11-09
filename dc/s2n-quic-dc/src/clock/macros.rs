// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

/// Implements a clock with standard millisecond-floored timer behavior.
///
/// This macro provides the standard clock implementation that floors sleep
/// durations to milliseconds to reduce timer churn. This is suitable for
/// most use cases where millisecond precision is sufficient.
macro_rules! impl_clock {
    () => {
        use super::SleepHandle;
        use core::{
            fmt,
            future::Future,
            pin::Pin,
            task::{Context, Poll},
            time::Duration,
        };
        use pin_project_lite::pin_project;
        use s2n_quic_core::{ready, time::Timestamp};
        use tracing::trace;

        #[derive(Clone, Debug)]
        pub struct Clock(Instant);

        impl Default for Clock {
            #[inline]
            fn default() -> Self {
                Self(Instant::now())
            }
        }

        impl s2n_quic_core::time::Clock for Clock {
            #[inline]
            fn get_time(&self) -> Timestamp {
                let time = self.0.elapsed();
                unsafe { Timestamp::from_duration(time) }
            }
        }

        pin_project!(
            pub struct Sleep {
                clock: Clock,
                #[pin]
                sleep: time::Sleep,
            }
        );

        impl s2n_quic_core::time::Clock for Sleep {
            #[inline]
            fn get_time(&self) -> Timestamp {
                self.clock.get_time()
            }
        }

        impl Future for Sleep {
            type Output = ();

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let this = self.project();
                ready!(core::future::Future::poll(this.sleep, cx));
                Poll::Ready(())
            }
        }

        impl super::Sleep for Sleep {
            #[inline]
            fn update(self: Pin<&mut Self>, target: Timestamp) {
                let target = unsafe { target.as_duration() };

                // floor the delay to milliseconds to reduce timer churn
                let delay = if target.as_millis() > 1 {
                    Duration::from_millis(target.as_millis() as u64)
                } else {
                    target
                };

                let target = self.clock.0 + delay;

                // if the clock has changed let the sleep future know
                trace!(update = ?target);
                self.project().sleep.reset(target);
            }
        }

        impl super::Clock for Sleep {
            #[inline]
            fn sleep(&self, amount: Duration) -> (SleepHandle, Timestamp) {
                self.clock.sleep(amount)
            }
        }

        impl fmt::Debug for Sleep {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct("Sleep")
                    .field("clock", &self.clock)
                    .field("sleep", &self.sleep)
                    .finish()
            }
        }

        impl super::Clock for Clock {
            #[inline]
            fn sleep(&self, amount: Duration) -> (SleepHandle, Timestamp) {
                let now = Instant::now();
                let sleep = sleep_until(now + amount);
                let sleep = Sleep {
                    clock: self.clone(),
                    sleep,
                };
                let sleep = Box::pin(sleep);
                let target = now.saturating_duration_since(self.0);
                let target = unsafe { Timestamp::from_duration(target) };
                (sleep, target)
            }
        }
    };
}

/// Implements a clock with microsecond-accurate timer behavior.
///
/// This macro provides a clock implementation with microsecond-level precision
/// for sleep operations. It uses a hybrid approach:
///
/// 1. For delays > 1ms: Uses the underlying timer (tokio::time::sleep) to sleep
///    until 1ms before the target, then switches to a cooperative spin-loop for
///    the final microsecond-level precision.
///
/// 2. For delays ≤ 1ms: Uses a cooperative spin-loop that repeatedly wakes and
///    yields the task. This allows other tasks to run while maintaining microsecond
///    precision.
///
/// ## Performance Considerations
///
/// The spin-loop approach for short delays will increase CPU usage and task wake
/// frequency. This is a necessary trade-off for applications that require
/// microsecond-accurate timing, such as the packet pacer in s2n-quic-dc.
///
/// ## State Machine
///
/// The Sleep future implements a state machine:
/// - `Initial`: Determine whether to sleep or spin based on remaining time
/// - `Sleeping`: Sleeping via tokio timer until 1ms before target
/// - `Spinning`: Cooperative spin-loop checking time and yielding
/// - `Done`: Target time reached
macro_rules! impl_microsecond_clock {
    () => {
        use super::SleepHandle;
        use core::{
            fmt,
            future::Future,
            pin::Pin,
            task::{Context, Poll},
            time::Duration,
        };
        use pin_project_lite::pin_project;
        use s2n_quic_core::{ready, time::Timestamp};
        use tracing::trace;

        #[derive(Clone, Debug)]
        pub struct Clock(Instant);

        impl Default for Clock {
            #[inline]
            fn default() -> Self {
                Self(Instant::now())
            }
        }

        impl s2n_quic_core::time::Clock for Clock {
            #[inline]
            fn get_time(&self) -> Timestamp {
                let time = self.0.elapsed();
                unsafe { Timestamp::from_duration(time) }
            }
        }

        pin_project!(
            pub struct Sleep {
                clock: Clock,
                target: Instant,
                #[pin]
                sleep: Option<time::Sleep>,
                state: SleepState,
            }
        );

        #[derive(Debug, Clone, Copy)]
        enum SleepState {
            Initial,
            Spinning,
            Sleeping,
            Done,
        }

        impl s2n_quic_core::time::Clock for Sleep {
            #[inline]
            fn get_time(&self) -> Timestamp {
                self.clock.get_time()
            }
        }

        impl Future for Sleep {
            type Output = ();

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let mut this = self.project();

                loop {
                    match this.state {
                        SleepState::Initial => {
                            let now = Instant::now();
                            let remaining = this.target.saturating_duration_since(now);

                            // For delays <= 1ms, use spin-loop with yields for microsecond accuracy
                            if remaining <= Duration::from_millis(1) {
                                *this.state = SleepState::Spinning;
                                continue;
                            } else {
                                // For longer delays, sleep until 1ms before target, then switch to spinning
                                let sleep_target = *this.target - Duration::from_millis(1);
                                this.sleep.set(Some(sleep_until(sleep_target)));
                                *this.state = SleepState::Sleeping;
                                continue;
                            }
                        }
                        SleepState::Spinning => {
                            let now = Instant::now();
                            if now >= *this.target {
                                *this.state = SleepState::Done;
                                return Poll::Ready(());
                            }

                            // Yield the task to be cooperative and avoid blocking the executor
                            // This allows other tasks to run while we wait for microsecond precision
                            cx.waker().wake_by_ref();
                            return Poll::Pending;
                        }
                        SleepState::Sleeping => {
                            if let Some(sleep) = this.sleep.as_mut().as_pin_mut() {
                                ready!(core::future::Future::poll(sleep, cx));
                            }
                            // Sleep completed, switch to spinning for final precision
                            *this.state = SleepState::Spinning;
                            continue;
                        }
                        SleepState::Done => {
                            return Poll::Ready(());
                        }
                    }
                }
            }
        }

        impl super::Sleep for Sleep {
            #[inline]
            fn update(mut self: Pin<&mut Self>, target: Timestamp) {
                let target_duration = unsafe { target.as_duration() };
                let new_target = self.clock.0 + target_duration;
                
                trace!(update = ?new_target);
                
                let mut this = self.as_mut().project();
                *this.target = new_target;
                *this.state = SleepState::Initial;
                this.sleep.set(None);
            }
        }

        impl super::Clock for Sleep {
            #[inline]
            fn sleep(&self, amount: Duration) -> (SleepHandle, Timestamp) {
                self.clock.sleep(amount)
            }
        }

        impl fmt::Debug for Sleep {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct("Sleep")
                    .field("clock", &self.clock)
                    .field("target", &self.target)
                    .field("state", &self.state)
                    .finish()
            }
        }

        impl super::Clock for Clock {
            #[inline]
            fn sleep(&self, amount: Duration) -> (SleepHandle, Timestamp) {
                let now = Instant::now();
                let target = now + amount;
                let sleep = Sleep {
                    clock: self.clone(),
                    target,
                    sleep: None,
                    state: SleepState::Initial,
                };
                let sleep = Box::pin(sleep);
                let target = now.saturating_duration_since(self.0);
                let target = unsafe { Timestamp::from_duration(target) };
                (sleep, target)
            }
        }
    };
}
