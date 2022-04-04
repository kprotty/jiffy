use crate::utils::CachePadded;
use std::{
    cell::{Cell, UnsafeCell},
    hint::spin_loop,
    mem::{drop, MaybeUninit},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread,
};

pub struct Queue<T> {
    semaphore: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
    head: CachePadded<Cell<usize>>,
    stored: Box<[AtomicBool]>,
    values: Box<[UnsafeCell<MaybeUninit<T>>]>,
}

unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Send> Sync for Queue<T> {}

impl<T> Queue<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            semaphore: CachePadded(AtomicUsize::new(capacity)),
            tail: CachePadded(AtomicUsize::new(0)),
            head: CachePadded(Cell::new(0)),
            stored: (0..capacity).map(|_| AtomicBool::new(false)).collect(),
            values: (0..capacity)
                .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
                .collect(),
        }
    }

    pub fn push(&self, value: T) -> Result<(), T> {
        let mut counter = 0usize;
        loop {
            let sema = self.semaphore.load(Ordering::Relaxed);
            let new_sema = match sema.checked_sub(1) {
                Some(new) => new,
                None => return Err(value),
            };

            if self
                .semaphore
                .compare_exchange(sema, new_sema, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }

            if cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
                counter = counter.wrapping_add(1);

                if counter <= 3 {
                    (0..(1 << counter)).for_each(|_| spin_loop());
                    continue;
                }

                if cfg!(windows) {
                    (0..(1 << counter.min(10))).for_each(|_| spin_loop());
                    continue;
                }
            }

            thread::yield_now();
        }

        Ok(unsafe {
            let tail = self.tail.fetch_add(1, Ordering::AcqRel);
            let index = tail % self.stored.len();

            self.values
                .get_unchecked(index)
                .get()
                .write(MaybeUninit::new(value));

            self.stored
                .get_unchecked(index)
                .store(true, Ordering::Release);
        })
    }

    pub unsafe fn pop(&self) -> Option<T> {
        let index = self.head.get();

        let stored = self.stored.get_unchecked(index);
        if !stored.load(Ordering::Acquire) {
            return None;
        }

        let value = self.values.get_unchecked(index);
        let value = value.get().read().assume_init();

        self.head.set((index + 1) % self.stored.len());
        stored.store(false, Ordering::Relaxed);

        self.semaphore.fetch_add(1, Ordering::Release);
        Some(value)
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        unsafe {
            let mut index = self.head.get();
            loop {
                let stored = self.stored.get_unchecked(index);
                if !stored.load(Ordering::Acquire) {
                    return;
                }

                let value = self.values.get_unchecked(index);
                drop(value.get().read().assume_init());

                stored.store(false, Ordering::Relaxed);
                index = (index + 1) % self.stored.len();
            }
        }
    }
}
