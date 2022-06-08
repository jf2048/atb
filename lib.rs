//! A simple lock-free, thread-safe SPSC triple buffer.
//!
//! Supports `no_std`, does no allocations and does not block.
//!
//! The buffer can be wrapped in an [`Arc`](std::sync::Arc) and sent to two different threads. The
//! producer thread can periodically call [`back_buffers`](AtomicTripleBuffer::back_buffers) to
//! write to the back buffer, and then call [`swap`](TBBackGuard::swap) to commit the back buffer.
//! Separately, the consumer thread can periodically call
//! [`front_buffer`](AtomicTripleBuffer::front_buffer) to read from the front buffer.
//!
//! Swapping is kept fast by simply updating a single atomic value storing the indexes of the front
//! and back buffers. No data is moved when swapping buffers.

#![cfg_attr(all(not(test), not(feature = "std")), no_std)]

use core::cell::UnsafeCell;

bitfield::bitfield! {
    #[derive(Copy, Clone, Eq, PartialEq)]
    #[repr(transparent)]
    struct BufferStatus(u16);
    impl Debug;
    #[inline]
    swap_pending, set_swap_pending: 0;
    #[inline]
    back_locked, set_back_locked: 1;
    #[inline]
    front_locked, set_front_locked: 2;
    #[inline]
    u8, front_index, set_front_index: 4, 3;
    #[inline]
    u8, work_index, set_work_index: 6, 5;
    #[inline]
    u8, pending_index, set_pending_index: 8, 7;
}

impl Default for BufferStatus {
    #[inline]
    fn default() -> Self {
        let mut status = Self(0);
        status.set_work_index(1);
        status.set_pending_index(2);
        status
    }
}

#[repr(transparent)]
#[derive(Debug)]
struct AtomicStatus(core::sync::atomic::AtomicU16);

impl Default for AtomicStatus {
    #[inline]
    fn default() -> Self {
        Self(core::sync::atomic::AtomicU16::new(
            BufferStatus::default().0,
        ))
    }
}

impl AtomicStatus {
    #[inline]
    fn fetch_update<F>(&self, mut f: F) -> Result<BufferStatus, BufferStatus>
    where
        F: FnMut(BufferStatus) -> Option<BufferStatus>,
    {
        use core::sync::atomic::Ordering;
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |s| {
                f(BufferStatus(s)).map(|s| s.0)
            })
            .map(BufferStatus)
            .map_err(BufferStatus)
    }
}

/// A container holding three instances of `T` that can be used as a queue.
///
/// The three instances correspond to three buffers: front buffer, back buffer, and pending
/// buffer. When the back buffer is written, it can be "committed" to swap it with the
/// pending buffer. The next time an attempt is made to read the front buffer, it will be swapped
/// with any pending buffer before reading.
///
/// The front buffer can be locked by one thread at a time and the back buffer can be locked by
/// another thread. This restriction ensures only one thread has access to each buffer at time,
/// making it safe to access the `AtomicTripleBuffer` from multiple threads without `T` being
/// [`Sync`](core::marker::Sync).
///
/// Locking and swapping are both accomplished by adjusting a single lock-free flag holding the
/// indexes of the buffers. No copying is done when swapping buffers.
#[derive(Debug)]
pub struct AtomicTripleBuffer<T> {
    buffers: [UnsafeCell<T>; 3],
    status: AtomicStatus,
}

unsafe impl<T: Send> Send for AtomicTripleBuffer<T> {}
unsafe impl<T: Send> Sync for AtomicTripleBuffer<T> {}

impl<T: Default> Default for AtomicTripleBuffer<T> {
    #[inline]
    fn default() -> Self {
        Self {
            buffers: [Default::default(), Default::default(), Default::default()],
            status: Default::default(),
        }
    }
}

/// An error that can be returned when trying to lock a buffer.
#[derive(Debug)]
pub enum TBLockError {
    AlreadyLocked,
}

#[cfg(feature = "std")]
impl std::error::Error for TBLockError {}

impl core::fmt::Display for TBLockError {
    fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::fmt::Result {
        fmt.write_str("buffer already locked")
    }
}

impl<T> AtomicTripleBuffer<T> {
    /// Creates a new `AtomicTripleBuffer` with all buffers set to `init`.
    pub fn new(init: T) -> Self
    where
        T: Clone,
    {
        Self {
            buffers: [
                UnsafeCell::new(init.clone()),
                UnsafeCell::new(init.clone()),
                UnsafeCell::new(init),
            ],
            status: Default::default(),
        }
    }
    /// Tries to lock the front buffer for access. Returns [`TBLockError::AlreadyLocked`] if the
    /// front buffer is already locked by another thread. If the pending buffer is locked, this
    /// method will atomically swap the pending and front buffers and unlock the pending buffer at
    /// the same time the front buffer is locked.
    pub fn front_buffer(&self) -> Result<TBFrontGuard<'_, T>, TBLockError> {
        let mut front_index = 0;
        self.status
            .fetch_update(|mut status| {
                if status.front_locked() {
                    return None;
                }
                status.set_front_locked(true);
                if status.swap_pending() {
                    status.set_swap_pending(false);
                    front_index = status.pending_index();
                    status.set_pending_index(status.front_index());
                    status.set_front_index(front_index);
                } else {
                    front_index = status.front_index();
                }
                Some(status)
            })
            .map_err(|_| TBLockError::AlreadyLocked)?;
        Ok(TBFrontGuard {
            cell: &self.buffers[front_index as usize],
            status: &self.status,
        })
    }
    /// Tries to lock the back buffer for access. Returns [`TBLockError::AlreadyLocked`] if the
    /// back buffer is already locked by another thread. Also allows access to the pending buffer
    /// if a frame is not pending.
    pub fn back_buffers(&self) -> Result<TBBackGuard<'_, T>, TBLockError> {
        let mut locked_status = BufferStatus::default();
        self.status
            .fetch_update(|mut status| {
                if status.back_locked() {
                    return None;
                }
                status.set_back_locked(true);
                locked_status = status;
                Some(status)
            })
            .map_err(|_| TBLockError::AlreadyLocked)?;
        Ok(TBBackGuard {
            bufs: self,
            locked_status,
        })
    }
}

/// An RAII guard holding a lock on the front buffer.
///
/// When the structure is dropped (falls out of scope), the front buffer will be unlocked. The
/// front buffer data can be accessed via the [`Deref`](core::ops::Deref) and
/// [`DerefMut`](core::ops::DerefMut) implementations.
#[derive(Debug)]
pub struct TBFrontGuard<'a, T> {
    cell: &'a UnsafeCell<T>,
    status: &'a AtomicStatus,
}

impl<T> core::ops::Deref for TBFrontGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.cell.get() }
    }
}

impl<T> core::ops::DerefMut for TBFrontGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.cell.get() }
    }
}

impl<T: core::fmt::Display> core::fmt::Display for TBFrontGuard<'_, T> {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        (**self).fmt(f)
    }
}

impl<T> Drop for TBFrontGuard<'_, T> {
    fn drop(&mut self) {
        self.status
            .fetch_update(|mut status| {
                status.set_front_locked(false);
                Some(status)
            })
            .ok();
    }
}

unsafe impl<T: Send> Send for TBFrontGuard<'_, T> {}
unsafe impl<T: Send + Sync> Sync for TBFrontGuard<'_, T> {}

/// An RAII guard holding a lock on the back buffer.
///
/// The back buffer can be accessed via the [`back`](TBBackGuard::back) and
/// [`back_mut`](TBBackGuard::back_mut) methods and committed with the [`swap`](TBBackGuard::swap)
/// method. After swapping, the back buffer will be in an indeterminate state and may be either the
/// previous front buffer or previous pending buffer. Usually callers should ensure the back buffer
/// is fully cleared or rewritten after acquiring the lock and before calling
/// [`swap`](TBBackGuard::swap). Dropping the guard structure without calling
/// [`swap`](TBBackGuard::swap) will unlock the back buffer, leaving it in the same state for the
/// next time it's locked.
///
/// The [`pending`](TBBackGuard::pending) and [`pending_mut`](TBBackGuard::pending_mut) methods can
/// also be used to access the pending buffer if it was not locked at the time this structure was
/// created. The primary reason to access the pending is to free data in the pending buffer without
/// having to wait for the next swap.
#[derive(Debug)]
pub struct TBBackGuard<'a, T> {
    bufs: &'a AtomicTripleBuffer<T>,
    locked_status: BufferStatus,
}

impl<T> TBBackGuard<'_, T> {
    /// Returns a reference to the back buffer.
    pub fn back(&self) -> &T {
        let index = self.locked_status.work_index() as usize;
        unsafe { &*self.bufs.buffers[index].get() }
    }
    /// Returns a mutable reference to the back buffer.
    pub fn back_mut(&mut self) -> &mut T {
        let index = self.locked_status.work_index() as usize;
        unsafe { &mut *self.bufs.buffers[index].get() }
    }
    /// Returns a reference to the pending buffer, or `None` if the pending buffer was locked at
    /// the time the structure was created.
    pub fn pending(&self) -> Option<&T> {
        if self.locked_status.swap_pending() {
            return None;
        }
        let index = self.locked_status.pending_index() as usize;
        Some(unsafe { &*self.bufs.buffers[index].get() })
    }
    /// Returns a mutable reference to the pending buffer, or `None` if the pending buffer was
    /// locked at the time the structure was created.
    pub fn pending_mut(&mut self) -> Option<&mut T> {
        if self.locked_status.swap_pending() {
            return None;
        }
        let index = self.locked_status.pending_index() as usize;
        Some(unsafe { &mut *self.bufs.buffers[index].get() })
    }
    /// Swaps the back buffer with the pending buffer and then locks the pending buffer if it was
    /// not already locked.
    pub fn swap(self) {
        self.bufs
            .status
            .fetch_update(|mut status| {
                status.set_back_locked(false);
                status.set_swap_pending(true);
                let pending_index = status.work_index();
                status.set_work_index(status.pending_index());
                status.set_pending_index(pending_index);
                Some(status)
            })
            .ok();
        core::mem::forget(self);
    }
}

impl<T: core::fmt::Display> core::fmt::Display for TBBackGuard<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.back().fmt(f)
    }
}

impl<T> Drop for TBBackGuard<'_, T> {
    fn drop(&mut self) {
        self.bufs
            .status
            .fetch_update(|mut status| {
                status.set_back_locked(false);
                Some(status)
            })
            .ok();
    }
}

unsafe impl<T: Send> Send for TBBackGuard<'_, T> {}
unsafe impl<T: Send + Sync> Sync for TBBackGuard<'_, T> {}

#[cfg(test)]
mod tests {
    use super::AtomicTripleBuffer;
    use std::sync::Arc;

    #[test]
    fn basic() {
        #[derive(Clone, Default)]
        struct Data {
            a: i32,
            b: i32,
        }

        let buf = Arc::new(AtomicTripleBuffer::<Data>::default());

        let b = buf.clone();
        let thread = std::thread::spawn(move || {
            let mut prev = Data::default();
            loop {
                let front = b.front_buffer().unwrap();
                assert!(front.a == front.b);
                assert!(front.a >= prev.a && front.b >= prev.b);
                if front.a >= 10000 {
                    break;
                }
                prev = (*front).clone();
            }
        });

        let mut data = Data::default();
        for _ in 0..10000 {
            data.a += 1;
            data.b += 1;
            let mut bufs = buf.back_buffers().unwrap();
            *bufs.back_mut() = data.clone();
            bufs.swap();
        }

        thread.join().unwrap();
    }
}
