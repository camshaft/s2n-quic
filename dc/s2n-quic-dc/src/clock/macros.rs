// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

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
