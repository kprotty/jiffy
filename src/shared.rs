use std::{
    cell::Cell,
    hint::spin_loop,
    marker::PhantomPinned,
    mem::drop,
    pin::Pin,
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
    thread,
};

pub unsafe trait StrictProvenance: Sized {
    fn addr(self) -> usize;
    fn with_addr(self, addr: usize) -> Self;
    fn map_addr(self, f: impl FnOnce(usize) -> usize) -> Self;
}

unsafe impl<T> StrictProvenance for *mut T {
    fn addr(self) -> usize {
        self as usize
    }

    fn with_addr(self, addr: usize) -> Self {
        addr as Self
    }

    fn map_addr(self, f: impl FnOnce(usize) -> usize) -> Self {
        self.with_addr(f(self.addr()))
    }
}

pub fn pinned<P: Default, T, F: FnOnce(Pin<&P>) -> T>(f: F) -> T {
    let pinnable = P::default();
    f(unsafe { Pin::new_unchecked(&pinnable) })
}

#[derive(Default)]
pub struct Backoff {
    counter: usize,
}

impl Backoff {
    pub fn try_yield_now(&mut self) -> bool {
        let mut max_spin = 0;
        if cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
            max_spin = 32;
        }

        self.counter < max_spin && {
            self.counter += 1;
            spin_loop();
            true
        }
    }

    pub fn yield_now(&mut self) {
        if cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
            self.counter = self.counter.wrapping_add(1);

            if self.counter <= 3 {
                (0..(1 << self.counter)).for_each(|_| spin_loop());
                return;
            }

            // On windows, it's better to backoff with spinning over yielding
            // (even with Sleep(0) or Sleep(1)) for some reason.
            if cfg!(windows) {
                (0..(1 << self.counter.min(10))).for_each(|_| spin_loop());
                return;
            }
        }

        thread::yield_now();
    }
}

pub struct Parker {
    thread: Cell<Option<thread::Thread>>,
    is_unparked: AtomicBool,
    _pinned: PhantomPinned,
}

impl Default for Parker {
    fn default() -> Self {
        Self {
            thread: Cell::new(Some(thread::current())),
            is_unparked: AtomicBool::new(false),
            _pinned: PhantomPinned,
        }
    }
}

impl Parker {
    pub fn park(&self) {
        while !self.is_unparked.load(Ordering::Acquire) {
            thread::park();
        }
    }

    pub unsafe fn unpark(&self) {
        let is_unparked = NonNull::from(&self.is_unparked).as_ptr();
        let thread = self.thread.take().unwrap();
        drop(self);

        (*is_unparked).store(true, Ordering::Release);
        thread.unpark();
    }
}

#[derive(Default)]
pub struct Waiter {
    pub next: Cell<Option<NonNull<Self>>>,
    pub parker: AtomicPtr<Parker>,
    _pinned: PhantomPinned,
}

impl Waiter {
    pub fn park(&self) {
        let mut p = self.parker.load(Ordering::Acquire);
        let notified = NonNull::dangling().as_ptr();

        if !ptr::eq(p, notified) {
            p = pinned::<Parker, _, _>(|parker| {
                let parker_ptr = NonNull::from(&*parker).as_ptr();
                assert!(!ptr::eq(parker_ptr, notified));

                if let Err(p) = self.parker.compare_exchange(
                    ptr::null_mut(),
                    parker_ptr,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    return p;
                }

                parker.park();
                self.parker.load(Ordering::Acquire)
            });
        }

        assert!(ptr::eq(p, notified));
        self.parker.store(ptr::null_mut(), Ordering::Relaxed);
    }

    pub unsafe fn unpark(&self) {
        let parker = NonNull::from(&self.parker).as_ptr();
        drop(self);

        let notified = NonNull::dangling().as_ptr();
        let parker = (*parker).swap(notified, Ordering::AcqRel);

        assert!(!ptr::eq(parker, notified));
        if !parker.is_null() {
            (*parker).unpark();
        }
    }
}
