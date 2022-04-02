use crate::{
    shared::{pinned, Backoff, StrictProvenance, Waiter},
    utils::CachePadded,
};
use std::{
    cell::UnsafeCell,
    collections::VecDeque,
    mem::swap,
    num::NonZeroUsize,
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
    thread,
};

fn num_cpus() -> NonZeroUsize {
    static CPUS: AtomicUsize = AtomicUsize::new(0);

    NonZeroUsize::new(CPUS.load(Ordering::Relaxed)).unwrap_or_else(|| {
        let cpus = thread::available_parallelism()
            .ok()
            .or(NonZeroUsize::new(1))
            .unwrap();

        CPUS.store(cpus.get(), Ordering::Relaxed);
        cpus
    })
}

struct Lock<T> {
    state: AtomicPtr<Waiter>,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for Lock<T> {}
unsafe impl<T: Send> Sync for Lock<T> {}

impl<T> Lock<T> {
    const LOCKED: usize = 1;

    const fn new(value: T) -> Self {
        Self {
            state: AtomicPtr::new(ptr::null_mut()),
            value: UnsafeCell::new(value),
        }
    }

    #[inline]
    fn with<F>(&self, f: impl FnOnce(&mut T) -> F) -> F {
        if !self.lock_fast() {
            self.lock_slow();
        }

        let result = f(unsafe { &mut *self.value.get() });

        if !self.unlock_fast() {
            self.unlock_slow();
        }

        result
    }

    #[inline(always)]
    fn lock_fast(&self) -> bool {
        self.state
            .compare_exchange_weak(
                ptr::null_mut::<Waiter>(),
                ptr::null_mut::<Waiter>().with_addr(Self::LOCKED),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    #[cold]
    fn lock_slow(&self) {
        pinned::<Waiter, _, _>(|waiter| {
            let mut spin = Backoff::default();
            let mut state = self.state.load(Ordering::Relaxed);

            loop {
                let mut backoff = Backoff::default();
                while state.addr() & Self::LOCKED == 0 {
                    if let Ok(_) = self.state.compare_exchange_weak(
                        state,
                        state.map_addr(|ptr| ptr | Self::LOCKED),
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    ) {
                        return;
                    }

                    backoff.yield_now();
                    state = self.state.load(Ordering::Relaxed);
                }

                let head = state.map_addr(|ptr| ptr & !Self::LOCKED);
                if head.is_null() && spin.try_yield_now() {
                    state = self.state.load(Ordering::Relaxed);
                    continue;
                }

                let waiter_ptr = NonNull::from(&*waiter).as_ptr();
                waiter.next.set(NonNull::new(head));

                if let Err(e) = self.state.compare_exchange_weak(
                    state,
                    waiter_ptr.map_addr(|ptr| ptr | Self::LOCKED),
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    state = e;
                    continue;
                }

                waiter.park();
                state = self.state.load(Ordering::Relaxed);
            }
        })
    }

    #[inline(always)]
    fn unlock_fast(&self) -> bool {
        self.state
            .compare_exchange(
                ptr::null_mut::<Waiter>().with_addr(Self::LOCKED),
                ptr::null_mut::<Waiter>(),
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    #[cold]
    fn unlock_slow(&self) {
        unsafe {
            let mut state = self.state.load(Ordering::Acquire);
            loop {
                assert_ne!(state.addr() & Self::LOCKED, 0);

                let waiter = state.map_addr(|ptr| ptr & !Self::LOCKED);
                assert!(!waiter.is_null());

                let next = (*waiter).next.get();
                let next = next.map(|ptr| ptr.as_ptr()).unwrap_or(ptr::null_mut());
                assert_eq!(next.addr() & Self::LOCKED, 0);

                match self.state.compare_exchange_weak(
                    state,
                    next,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return (*waiter).unpark(),
                    Err(e) => state = e,
                }
            }
        }
    }
}

struct Producer<T> {
    pending: AtomicBool,
    deque: Lock<VecDeque<T>>,
}

impl<T> Producer<T> {
    fn new() -> Self {
        Self {
            pending: AtomicBool::new(false),
            deque: Lock::new(VecDeque::new()),
        }
    }

    fn push(&self, item: T) {
        self.deque.with(|deque| deque.push_back(item));
        self.pending.store(true, Ordering::Release);
    }

    fn swap(&self, new: &mut VecDeque<T>) {
        if self.pending.load(Ordering::Acquire) {
            self.pending.store(false, Ordering::Relaxed);
            self.deque.with(|deque| swap(new, deque));
        }
    }
}

pub struct MpscQueue<T> {
    index: AtomicUsize,
    consumer: UnsafeCell<VecDeque<T>>,
    producers: Box<[CachePadded<Producer<T>>]>,
}

unsafe impl<T: Send> Send for MpscQueue<T> {}
unsafe impl<T: Send> Sync for MpscQueue<T> {}

impl<T> Default for MpscQueue<T> {
    fn default() -> Self {
        Self {
            index: AtomicUsize::new(0),
            consumer: UnsafeCell::new(VecDeque::new()),
            producers: (0..num_cpus().get())
                .map(|_| CachePadded(Producer::new()))
                .collect(),
        }
    }
}

impl<T> MpscQueue<T> {
    pub fn push(&self, value: T) {
        let index = self.index.fetch_add(1, Ordering::Relaxed);
        assert_ne!(self.producers.len(), 0);

        let producer = &self.producers[index % self.producers.len()];
        producer.push(value);
    }

    pub unsafe fn pop(&self) -> Option<T> {
        let consumer = &mut *self.consumer.get();
        if let Some(item) = consumer.pop_front() {
            return Some(item);
        }

        for producer in self.producers.iter() {
            producer.swap(consumer);
            if consumer.len() > 0 {
                break;
            }
        }

        consumer.pop_front()
    }
}
