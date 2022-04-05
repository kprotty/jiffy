use crate::utils::{StrictProvenance, CachePadded};
use std::{
    thread,
    pin::Pin,
    alloc::{alloc, dealloc, Layout},
    ptr::{self, NonNull},
    marker::PhantomPinned,
    hint::spin_loop,
    cell::{UnsafeCell, Cell},
    mem::{drop, MaybeUninit},
    sync::atomic::{AtomicPtr, AtomicBool, AtomicUsize, Ordering},
};

#[derive(Default)]
struct Backoff(usize);

impl Backoff {
    fn is_completed(&self) -> bool {
        self.0 > 3
    }

    fn yield_now(&mut self) {
        self.0 = self.0.wrapping_add(1);
        if !self.is_completed() || cfg!(windows) {
            for _ in 0..(1 << self.0.min(10)) {
                spin_loop();
            }
        } else {
            thread::yield_now();
        }
    }
}

fn pinned<P: Default, T, F: FnOnce(Pin<&P>) -> T>(f: F) -> T {
    let pinnable = P::default();
    f(unsafe { Pin::new_unchecked(&pinnable) })
}

#[derive(Default)]
struct Event {
    thread: Cell<Option<thread::Thread>>,
    is_set: AtomicBool,
    _pinned: PhantomPinned,
}

impl Event {
    fn with<F>(f: impl FnOnce(Pin<&Self>) -> F) -> F {
        pinned::<Self, _, _>(|event| {
            event.thread.set(Some(thread::current()));
            f(event)
        })
    }

    fn wait(&self) {
        while !self.is_set.load(Ordering::Acquire) {
            thread::park();
        }
    }

    fn set(&self) {
        let thread = self.thread.take().unwrap();
        self.is_set.store(true, Ordering::Release);
        thread.unpark();
    }
}

#[derive(Default)]
struct Parker {
    event: AtomicPtr<Event>,
}

impl Parker {
    const fn new() -> Self {
        Self {
            event: AtomicPtr::new(ptr::null_mut()),
        }
    }

    #[cold]
    fn park(&self) {
        let mut ev = self.event.load(Ordering::Acquire);
        let notified = NonNull::dangling().as_ptr();

        let mut spin = Backoff::default();
        while !ptr::eq(ev, notified) && !spin.is_completed() {
            spin.yield_now();
            ev = self.event.load(Ordering::Acquire);
        } 

        if !ptr::eq(ev, notified) {
            Event::with(|event| {
                let event_ptr = NonNull::from(&*event).as_ptr();
                assert!(!ptr::eq(event_ptr, notified));

                if let Ok(_) = self.event.compare_exchange(
                    ptr::null_mut(),
                    event_ptr,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    event.wait();
                }

                ev = self.event.load(Ordering::Acquire);
            });
        }

        assert!(ptr::eq(ev, notified));
        self.event.store(ptr::null_mut(), Ordering::Relaxed);
    }

    #[cold]
    fn unpark(&self) {
        self.event
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |ev| {
                let notified = NonNull::dangling().as_ptr();
                if ptr::eq(ev, notified) {
                    None
                } else {
                    Some(notified)
                }
            })
            .map(|ev| unsafe {
                if !ev.is_null() {
                    (*ev).set();
                }
            })
            .unwrap_or(())
    }
}

#[repr(align(2))]
#[derive(Default)]
struct Waiter {
    next: Cell<Option<NonNull<Self>>>,
    parker: Parker,
}

struct Lock<T> {
    state: AtomicPtr<Waiter>,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for Lock<T> {}
unsafe impl<T: Send> Sync for Lock<T> {}

impl<T: Default> Default for Lock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

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
        if let Err(_) = self.state.compare_exchange_weak(
            ptr::null_mut::<Waiter>(),
            ptr::null_mut::<Waiter>().with_addr(Self::LOCKED),
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            self.lock_slow();
        }

        let result = f(unsafe { &mut *self.value.get() });

        if let Err(_) = self.state.compare_exchange(
            ptr::null_mut::<Waiter>().with_addr(Self::LOCKED),
            ptr::null_mut::<Waiter>(),
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            unsafe { self.unlock_slow() };
        }

        result
    }

    #[cold]
    fn lock_slow(&self) {
        pinned::<Waiter, _, _>(|waiter| {
            let mut spin = Backoff::default();
            let mut state = self.state.load(Ordering::Relaxed);

            loop {
                let mut backoff = Backoff::default();
                while state.addr() & Self::LOCKED == 0 {
                    if let Ok(_) = self.state.compare_exchange(
                        state,
                        state.map_addr(|addr| addr | Self::LOCKED),
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    ) {
                        return;
                    }

                    backoff.yield_now();
                    state = self.state.load(Ordering::Relaxed);
                }

                let head = NonNull::new(state.map_addr(|addr| addr & !Self::LOCKED));
                if head.is_none() && !spin.is_completed() {
                    spin.yield_now();
                    state = self.state.load(Ordering::Relaxed);
                    continue;
                }
                
                let waiter_ptr = NonNull::from(&*waiter).as_ptr();
                waiter.next.set(head);

                if let Err(e) = self.state.compare_exchange_weak(
                    state,
                    waiter_ptr.map_addr(|addr| addr | Self::LOCKED),
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    state = e;
                    continue;
                }

                waiter.parker.park();
                state = self.state.load(Ordering::Relaxed);
            }
        }) 
    }

    #[cold]
    unsafe fn unlock_slow(&self) {
        loop {
            let state = self.state.load(Ordering::Acquire);
            assert!(state.addr() & Self::LOCKED != 0);

            let head = state.map_addr(|addr| addr & !Self::LOCKED);
            assert!(!head.is_null());

            let next = (*head).next.get().map(|p| p.as_ptr());
            let new_state = next.unwrap_or(ptr::null_mut());

            match self.state.compare_exchange(
                state,
                new_state,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return (*head).parker.unpark(),
                Err(_) => spin_loop(),
            }
        }
    }
}

#[derive(Default)]
struct WaitQueue {
    queue: Lock<Option<NonNull<Waiter>>>,
    pending: AtomicBool,
}

impl WaitQueue {
    const fn new() -> Self {
        Self {
            queue: Lock::new(None),
            pending: AtomicBool::new(false),
        }
    }

    fn park_if(&self, condition: impl FnOnce() -> bool) {
        pinned::<Waiter, _, _>(|waiter| {
            if self.queue.with(|queue| {
                if !self.pending.load(Ordering::Relaxed) {
                    self.pending.store(true, Ordering::SeqCst);
                }

                if !condition() {
                    if queue.is_none() {
                        self.pending.store(false, Ordering::Relaxed);
                    }
                    return false;
                }

                waiter.next.set(*queue);
                *queue = Some(NonNull::from(&*waiter));
                true
            }) {
                waiter.parker.park();
            }
        });
    }

    fn unpark_one(&self) {
        if !self.pending.load(Ordering::SeqCst) {
            return;
        }

        unsafe {
            if let Some(waiter) = self.queue.with(|queue| {
                if !self.pending.load(Ordering::Relaxed) {
                    return None;
                }

                let waiter = (*queue)?;
                *queue = waiter.as_ref().next.get();

                if queue.is_none() {
                    self.pending.store(false, Ordering::Relaxed);
                }

                Some(waiter)
            }) {
                waiter.as_ref().parker.unpark();
            }
        }
    }
}

#[derive(Default)]
struct Producer {
    position: AtomicUsize,
    wait_queue: WaitQueue,
}

#[derive(Default)]
struct Consumer {
    position: Cell<usize>,
    parker: Parker,
}

struct Slot<T> {
    value: UnsafeCell<MaybeUninit<T>>,
    stored: AtomicBool,
}

pub struct Queue<T> {
    producer: CachePadded<Producer>,
    consumer: CachePadded<Consumer>,
    slots: CachePadded<Box<[Slot<T>]>>,
}

unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Send> Sync for Queue<T> {}

impl<T> Queue<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            producer: CachePadded(Producer {
                position: AtomicUsize::new(0),
                wait_queue: WaitQueue::new(),
            }),
            consumer: CachePadded(Consumer {
                position: Cell::new(0),
                parker: Parker::new(),
            }),
            slots: CachePadded({
                (0..capacity.max(1).next_power_of_two())
                    .map(|_| Slot {
                        value: UnsafeCell::new(MaybeUninit::uninit()),
                        stored: AtomicBool::new(false),
                    })
                    .collect()
            }),
        }
    }

    pub fn try_send(&self, item: T) -> Result<(), T> {
        let mut backoff = Backoff::default();
        let mask = self.slots.len() - 1;

        loop {
            let pos = self.producer.position.load(Ordering::Acquire);
            let slot = unsafe { self.slots.get_unchecked(pos & mask) };

            if slot.stored.load(Ordering::Acquire) {
                let current_pos = self.producer.position.load(Ordering::Relaxed);
                if pos == current_pos {
                    return Err(item);
                }

                spin_loop();
                continue;
            }

            if let Err(_) = self.producer.position.compare_exchange(
                pos,
                pos.wrapping_add(1),
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                backoff.yield_now();
                continue;
            }

            return Ok(unsafe {
                slot.value.get().write(MaybeUninit::new(item));
                slot.stored.store(true, Ordering::Release);
                self.consumer.parker.unpark();
            });
        }
    }

    pub fn send(&self, mut item: T) {
        loop {
            match self.try_send(item) {
                Ok(_) => return,
                Err(e) => item = e,
            }

            self.producer.wait_queue.park_if(|| {
                let mask = self.slots.len() - 1;
                let pos = self.producer.position.load(Ordering::SeqCst);
                let slot = unsafe { self.slots.get_unchecked(pos & mask) };

                if slot.stored.load(Ordering::Acquire) {
                    let current_pos = self.producer.position.load(Ordering::Relaxed);
                    return pos == current_pos;
                }

                false
            });
        }
    }

    pub unsafe fn try_recv(&self) -> Option<T> {
        let mask = self.slots.len() - 1;
        let pos = self.consumer.position.get();
        let slot = self.slots.get_unchecked(pos & mask);

        if slot.stored.load(Ordering::Acquire) {
            let value = slot.value.get().read().assume_init();
            slot.stored.store(false, Ordering::SeqCst);

            self.producer.wait_queue.unpark_one();

            self.consumer.position.set(pos.wrapping_add(1));
            return Some(value);
        }

        None
    }

    pub unsafe fn recv(&self) -> T {
        loop {
            match self.try_recv() {
                Some(value) => return value,
                None => self.consumer.parker.park(),
            }
        }
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        while let Some(value) = unsafe { self.try_recv() } {
            drop(value);
        }
    }
}