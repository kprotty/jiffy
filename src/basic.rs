use crate::utils::CachePadded;
use std::{
    cell::{Cell, UnsafeCell},
    mem::{drop, MaybeUninit},
    sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering},
};

pub struct Queue<T> {
    semaphore: CachePadded<AtomicIsize>,
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
            semaphore: CachePadded(AtomicIsize::new(capacity.try_into().unwrap())),
            tail: CachePadded(AtomicUsize::new(0)),
            head: CachePadded(Cell::new(0)),
            stored: (0..capacity).map(|_| AtomicBool::new(false)).collect(),
            values: (0..capacity)
                .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
                .collect(),
        }
    }

    pub fn push(&self, value: T) -> Result<(), T> {
        if !self.sem_try_wait() {
            return Err(value);
        }

        Ok(unsafe {
            let tail = self.tail.fetch_add(1, Ordering::Relaxed);
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

        self.sem_post();
        Some(value)
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
impl<T> Queue<T> {
    fn sem_try_wait(&self) -> bool {
        loop {
            let sema = self.semaphore.fetch_sub(1, Ordering::Acquire);
            if sema > 0 {
                return true;
            }

            (0..32).for_each(|_| std::hint::spin_loop());

            let sema = self.semaphore.fetch_add(1, Ordering::Relaxed);
            if sema < 0 {
                return false;
            }

            std::thread::yield_now();
        }
    }

    fn sem_post(&self) {
        self.semaphore.fetch_add(1, Ordering::Release);
    }
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
impl<T> Queue<T> {
    #[inline]
    fn fetch_update(
        &self,
        ordering: Ordering,
        mut f: impl FnMut(isize) -> Option<isize>,
    ) -> Result<isize, isize> {
        loop {
            let sema = self.semaphore.load(Ordering::Relaxed);
            let new_sema = match f(sema) {
                Some(new) => new,
                None => return Err(sema),
            };

            match self
                .semaphore
                .compare_exchange(sema, new_sema, ordering, Ordering::Relaxed)
            {
                Ok(_) => return Ok(sema),
                Err(_) => std::thread::yield_now(),
            }
        }
    }

    fn sem_try_wait(&self) -> bool {
        self.fetch_update(Ordering::Acquire, |sema| {
            if sema == 0 {
                None
            } else {
                Some(sema - 1)
            }
        })
        .is_ok()
    }

    fn sem_post(&self) {
        self.fetch_update(Ordering::Release, |sema| sema + 1)
            .unwrap();
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
