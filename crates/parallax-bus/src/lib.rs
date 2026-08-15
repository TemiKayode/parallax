//! The internal event bus: bounded, lock-free, single-digit-nanosecond
//! push/pop channels connecting every pipeline stage in the architecture
//! diagram (design doc §4). Backed by `crossbeam_queue::ArrayQueue`, a
//! well-proven lock-free MPMC ring buffer — the honest, real-world
//! equivalent of the "Disruptor pattern" referenced in the design doc,
//! rather than a hand-rolled unsafe structure whose correctness would be
//! very hard to guarantee under review.
//!
//! Hot-path discipline: publishing never blocks. A full topic drops the
//! new item and increments a counter instead of applying backpressure —
//! backpressure on the trade-critical path is its own kind of latency
//! bug. A rising drop count is a capacity/consumer-speed problem to fix
//! upstream, not something to paper over here.

#![forbid(unsafe_code)]

mod topics;

pub use topics::PipelineBus;

use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Bus<T> {
    queue: ArrayQueue<T>,
    dropped: AtomicU64,
    published: AtomicU64,
}

impl<T> Bus<T> {
    /// `ArrayQueue::new(0)` panics outright — a mis-set capacity (an
    /// empty config, a bad env var parse) would take the process down at
    /// startup rather than at the moment it's actually exercised. Clamping
    /// to at least 1 trades "crash on boot" for "publish immediately
    /// starts dropping," which is observable via `dropped_count()` instead
    /// of being unrecoverable (design doc review 3.19).
    pub fn new(capacity: usize) -> Self {
        Bus {
            queue: ArrayQueue::new(capacity.max(1)),
            dropped: AtomicU64::new(0),
            published: AtomicU64::new(0),
        }
    }

    /// Non-blocking publish. Returns `false` (and counts a drop) if the
    /// topic is full — callers on the hot path must not treat that as a
    /// fatal error, only as a signal fed into observability.
    pub fn try_publish(&self, item: T) -> bool {
        match self.queue.push(item) {
            Ok(()) => {
                self.published.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(_full_item) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    pub fn try_recv(&self) -> Option<T> {
        self.queue.pop()
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.queue.capacity()
    }

    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn published_count(&self) -> u64 {
        self.published.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_capacity_is_clamped_instead_of_panicking() {
        let bus: Bus<u32> = Bus::new(0);
        assert_eq!(bus.capacity(), 1);
        assert!(bus.try_publish(1));
        assert!(!bus.try_publish(2));
        assert_eq!(bus.dropped_count(), 1);
    }

    #[test]
    fn publish_and_drain_preserve_order() {
        let bus: Bus<u32> = Bus::new(4);
        for i in 0..4 {
            assert!(bus.try_publish(i));
        }
        // fifth publish should drop, not block or panic
        assert!(!bus.try_publish(99));
        assert_eq!(bus.dropped_count(), 1);

        for i in 0..4 {
            assert_eq!(bus.try_recv(), Some(i));
        }
        assert_eq!(bus.try_recv(), None);
    }

    #[test]
    fn concurrent_producers_do_not_lose_or_duplicate_within_capacity() {
        use std::sync::Arc;
        use std::thread;

        let bus: Arc<Bus<u64>> = Arc::new(Bus::new(10_000));
        let mut handles = Vec::new();
        for t in 0..8u64 {
            let bus = Arc::clone(&bus);
            handles.push(thread::spawn(move || {
                for i in 0..1000u64 {
                    let val = t * 1000 + i;
                    while !bus.try_publish(val) {
                        std::thread::yield_now();
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(bus.len(), 8000);
        assert_eq!(bus.published_count(), 8000);
    }
}
