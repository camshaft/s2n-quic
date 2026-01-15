use arc_swap::ArcSwap;
use aws_lc_rs::aead::{self, LessSafeKey, UnboundKey};
use bytes::Bytes;
use std::{
    future::poll_fn,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    task::{Poll, Waker},
};

#[derive(Clone)]
pub struct RawKey(Bytes);

#[derive(Clone)]
pub struct Key(Arc<Inner>);

struct Inner {
    state: ArcSwap<State>,
    byte_limit: u64,
    record_limit: u64,
    pending_rotation: AtomicBool,
    algorithm: &'static aead::Algorithm,
}

fn generate_key(algorithm: &'static aead::Algorithm) -> (LessSafeKey, RawKey) {
    let mut dest = vec![0u8; algorithm.key_len()];
    aws_lc_rs::rand::fill(&mut dest).unwrap();
    let key = UnboundKey::new(algorithm, &dest).unwrap();
    let key = LessSafeKey::new(key);
    let raw_key = RawKey(Bytes::from(dest));
    (key, raw_key)
}

impl Key {
    pub fn encrypt<R, E>(
        &self,
        f: impl FnOnce(&LessSafeKey, u64) -> Result<usize, E>,
    ) -> Result<usize, E> {
        let state = self.0.state.load();
        let nonce = state.nonce.fetch_add(1, Ordering::Relaxed);
        let res = f(&state.key, nonce);

        if let Ok(size) = &res {
            let prev = state
                .encrypted_bytes
                .fetch_add(*size as u64, Ordering::Relaxed);
            let next = prev + *size as u64;
            let is_at_limit = next > self.0.byte_limit || nonce > self.0.record_limit;
            if is_at_limit && !self.0.pending_rotation.swap(true, Ordering::Relaxed) {
                state.waker.wake_by_ref();
            }
        }

        res
    }

    async fn rotate(&self) {
        loop {
            let waker = poll_fn(|cx| {
                if self.0.pending_rotation.swap(false, Ordering::Relaxed) {
                    Poll::Ready(cx.waker().clone())
                } else {
                    Poll::Pending
                }
            })
            .await;

            let (key, raw_key) = generate_key(self.0.algorithm);
            let state = State {
                raw_key,
                key,
                nonce: AtomicU64::new(0),
                encrypted_bytes: AtomicU64::new(0),
                waker,
            };

            self.0.state.store(Arc::new(state));
        }
    }
}

struct State {
    raw_key: RawKey,
    key: LessSafeKey,
    nonce: AtomicU64,
    encrypted_bytes: AtomicU64,
    waker: Waker,
}
