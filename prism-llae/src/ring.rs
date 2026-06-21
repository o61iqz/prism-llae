//! Wait-free SPSC ring buffer of `f32` bridging the capture and render threads.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Inner {
    buf: Box<[UnsafeCell<f32>]>,
    mask: usize,
    head: AtomicUsize, // write cursor (producer)
    tail: AtomicUsize, // read cursor (consumer)
}

// SAFETY: disjoint access enforced by the atomic cursors (acquire/release)
unsafe impl Sync for Inner {}
unsafe impl Send for Inner {}

pub struct Producer {
    inner: Arc<Inner>,
}

pub struct Consumer {
    inner: Arc<Inner>,
}

unsafe impl Send for Producer {}
unsafe impl Send for Consumer {}

// capacity rounded up to a power of two >= min_capacity
pub fn ring(min_capacity: usize) -> (Producer, Consumer) {
    let cap = min_capacity.max(2).next_power_of_two();
    let mut v = Vec::with_capacity(cap);
    for _ in 0..cap {
        v.push(UnsafeCell::new(0.0));
    }
    let inner = Arc::new(Inner {
        buf: v.into_boxed_slice(),
        mask: cap - 1,
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
    });
    (
        Producer {
            inner: inner.clone(),
        },
        Consumer { inner },
    )
}

impl Inner {
    #[inline]
    fn capacity(&self) -> usize {
        self.mask + 1
    }
}

impl Producer {
    pub fn free(&self) -> usize {
        let head = self.inner.head.load(Ordering::Relaxed);
        let tail = self.inner.tail.load(Ordering::Acquire);
        self.inner.capacity() - head.wrapping_sub(tail)
    }

    // push as many as fit; returns count written
    pub fn push(&self, data: &[f32]) -> usize {
        let head = self.inner.head.load(Ordering::Relaxed);
        let tail = self.inner.tail.load(Ordering::Acquire);
        let free = self.inner.capacity() - head.wrapping_sub(tail);
        let n = free.min(data.len());
        for (i, &s) in data.iter().take(n).enumerate() {
            let idx = head.wrapping_add(i) & self.inner.mask;
            // SAFETY: idx within the producer-owned free region
            unsafe {
                *self.inner.buf[idx].get() = s;
            }
        }
        self.inner
            .head
            .store(head.wrapping_add(n), Ordering::Release);
        n
    }
}

impl Consumer {
    pub fn available(&self) -> usize {
        let tail = self.inner.tail.load(Ordering::Relaxed);
        let head = self.inner.head.load(Ordering::Acquire);
        head.wrapping_sub(tail)
    }

    // pop into `out`; returns count read
    pub fn pop(&self, out: &mut [f32]) -> usize {
        let tail = self.inner.tail.load(Ordering::Relaxed);
        let head = self.inner.head.load(Ordering::Acquire);
        let avail = head.wrapping_sub(tail);
        let n = avail.min(out.len());
        for (i, slot) in out.iter_mut().take(n).enumerate() {
            let idx = tail.wrapping_add(i) & self.inner.mask;
            // SAFETY: idx within the consumer-owned readable region
            unsafe {
                *slot = *self.inner.buf[idx].get();
            }
        }
        self.inner
            .tail
            .store(tail.wrapping_add(n), Ordering::Release);
        n
    }

    // discard up to `n` samples; returns count dropped
    pub fn skip(&self, n: usize) -> usize {
        let tail = self.inner.tail.load(Ordering::Relaxed);
        let head = self.inner.head.load(Ordering::Acquire);
        let avail = head.wrapping_sub(tail);
        let dropped = avail.min(n);
        self.inner
            .tail
            .store(tail.wrapping_add(dropped), Ordering::Release);
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let (p, c) = ring(8);
        assert_eq!(p.push(&[1.0, 2.0, 3.0]), 3);
        let mut out = [0.0; 4];
        assert_eq!(c.pop(&mut out), 3);
        assert_eq!(&out[..3], &[1.0, 2.0, 3.0]);
    }

    #[test]
    fn fills_and_wraps() {
        let (p, c) = ring(4); // capacity 4
        assert_eq!(p.push(&[1.0, 2.0, 3.0, 4.0, 5.0]), 4);
        let mut out = [0.0; 2];
        assert_eq!(c.pop(&mut out), 2);
        assert_eq!(p.push(&[6.0, 7.0]), 2);
        let mut out2 = [0.0; 8];
        assert_eq!(c.pop(&mut out2), 4);
        assert_eq!(&out2[..4], &[3.0, 4.0, 6.0, 7.0]);
    }
}
