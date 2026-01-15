use std::{
    cell::UnsafeCell,
    marker::PhantomData,
    pin::pin,
    sync::atomic::{AtomicI32, Ordering},
    task::{Poll, Waker},
};

/// State for a continuation that is stored on the future stack
pub struct Continuation {
    waker: Waker,
    result: AtomicI32,
}

impl Continuation {
    async fn new() -> Self {
        let waker = core::future::poll_fn(|cx| Poll::Ready(cx.waker().clone())).await;
        Self {
            waker,
            result: AtomicI32::new(i32::MIN),
        }
    }
}

pub struct Token(*mut Continuation);

/// SAFETY: The continuation can be notified from other threads
unsafe impl Send for Token {}

impl Token {
    pub unsafe fn notify(self, value: i32) {
        notify(self.0, value);
    }

    pub fn from_ffi(ptr: *mut Continuation) -> Self {
        Self(ptr)
    }

    pub fn to_ffi(self) -> *mut Continuation {
        self.0
    }
}

/// Notifies a continuation with a result value
///
/// # Safety
///
/// The caller must ensure that:
/// - `handle` is a valid pointer to a `Continuation` that was created by `wait()`
/// - The `Continuation` is still alive (the future has not completed or been dropped)
/// - This function is called at most once per continuation
/// - The pointer was obtained from the same `wait()` call that is currently pending
///
/// # Cross-thread safety
///
/// This function is thread-safe and can be called from any thread. The `Waker`
/// will be invoked on the calling thread, which will schedule the continuation
/// to resume on its original executor.
pub unsafe fn notify(handle: *mut Continuation, value: i32) {
    debug_assert_ne!(handle, std::ptr::null_mut());
    debug_assert_ne!(value, i32::MIN);

    // We need mutable access to set the result and take the waker
    // This is safe because:
    // 1. The continuation is pinned and won't move
    // 2. Only one thread will call notify() due to the "at most once" requirement
    // 3. The future polling is synchronized through the executor
    let continuation = unsafe { &mut *handle };

    continuation.result.store(value, Ordering::Release);

    continuation.waker.wake_by_ref();
}

/// Waits for an asynchronous operation to complete
///
/// This function creates a continuation on the stack, pins it, and passes a pointer
/// to the `operation` callback. The operation should store this pointer and later
/// call `notify()` or `notify_batch()` to complete the continuation.
///
/// # Example
///
/// ```ignore
/// let result = wait(|continuation_ptr| {
///     // Store continuation_ptr somewhere
///     // Later, from potentially another thread:
///     // unsafe { notify(continuation_ptr, 42) }
/// }).await;
/// ```
///
/// # Panics
///
/// This function will panic if the future is dropped before the continuation completes.
/// This is enforced by the `Guard` which ensures that the continuation must be
/// explicitly completed via `notify()`.
pub async fn wait(operation: impl FnOnce(Token) -> Result<(), i32>) -> i32 {
    // Create the continuation state on the stack
    let mut continuation = UnsafeCell::new(Continuation::new().await);

    let guard = Guard::new();

    let continuation = pin!(continuation);

    // Get a stable pointer to the pinned continuation
    let ptr: *mut Continuation = continuation.get();

    // Pass the pointer to the operation
    if let Err(err) = operation(Token(ptr)) {
        guard.forget();
        return err;
    }

    // Poll until we get a result
    let result = core::future::poll_fn(|cx| {
        // Safety: We have exclusive access through the pinned mutable reference
        let continuation = unsafe { &mut *ptr };

        let result = continuation.result.load(Ordering::Acquire);
        if result != i32::MIN {
            return Poll::Ready(result);
        }

        debug_assert!(continuation.waker.will_wake(cx.waker()));
        Poll::Pending
    })
    .await;

    guard.forget();

    result
}

/// Guard that ensures the continuation completes before unwinding
///
/// This guard will panic if dropped, ensuring that continuations are not
/// abandoned mid-flight. The `forget()` method must be called to successfully
/// complete a continuation.
struct Guard(PhantomData<*const ()>);

impl Guard {
    fn new() -> Self {
        Guard(PhantomData)
    }

    fn forget(self) {
        core::mem::forget(self);
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        panic!("continuation guard dropped - continuation was not completed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{future::Future, pin::pin, thread, time::Duration};

    #[tokio::test]
    async fn test_basic_wait_and_notify() {
        let result = wait(|ptr| {
            // Simulate an async operation that completes immediately
            unsafe {
                ptr.notify(42);
                Ok(())
            }
        })
        .await;

        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_wait_and_notify_different_values() {
        for expected in [0, 1, -1, i32::MAX, i32::MIN] {
            let result = wait(|ptr| unsafe {
                ptr.notify(expected);
                Ok(())
            })
            .await;

            assert_eq!(result, expected);
        }
    }

    #[tokio::test]
    async fn test_cross_thread_notify() {
        let result = wait(|ptr| {
            // Spawn a thread to notify from a different thread
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(10));
                unsafe {
                    ptr.notify(123);
                }
            });
            Ok(())
        })
        .await;

        assert_eq!(result, 123);
    }

    #[tokio::test]
    async fn test_delayed_cross_thread_notify() {
        let delays = [10, 50, 100];

        for delay_ms in delays {
            let result = wait(|ptr| {
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(delay_ms));
                    unsafe {
                        ptr.notify(delay_ms as i32);
                    }
                });
                Ok(())
            })
            .await;

            assert_eq!(result, delay_ms as i32);
        }
    }

    #[tokio::test]
    #[should_panic(expected = "continuation guard dropped - continuation was not completed")]
    async fn test_guard_panics_on_drop() {
        let mut fut = wait(|_ptr| {
            // Don't call notify - this should panic when the guard is dropped
            Ok(())
        });

        let mut fut = pin!(fut);

        let waker = core::future::poll_fn(|cx| Poll::Ready(cx.waker().clone())).await;
        let mut cx = core::task::Context::from_waker(&waker);

        // Poll the future once to register everything
        let _ = fut.as_mut().poll(&mut cx);
    }
}
