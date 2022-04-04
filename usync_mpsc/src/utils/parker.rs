use super::Backoff;
use std::{
    cell::Cell,
    marker::PhantomPinned,
    pin::Pin,
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
    thread,
    time::Instant,
};

#[derive(Default)]
struct Event {
    thread: Cell<Option<thread::Thread>>,
    is_set: AtomicBool,
    _pinned: PhantomPinned,
}

impl Event {
    fn with<F>(f: impl FnOnce(Pin<&Self>) -> F) -> F {
        let event = Event::default();
        event.thread.set(Some(thread::current()));
        f(unsafe { Pin::new_unchecked(&event) })
    }

    fn wait(self: Pin<&Self>, deadline: Option<Instant>) -> bool {
        loop {
            if self.is_set.load(Ordering::Acquire) {
                return true;
            }

            match deadline {
                None => thread::park(),
                Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                    Some(until_deadline) => thread::park_timeout(until_deadline),
                    None => return false,
                },
            }
        }
    }

    unsafe fn set(self: Pin<&Self>) {
        let thread = self.thread.take().expect("event without thread");
        self.is_set.store(true, Ordering::Release);
        thread.unpark();
    }
}

#[derive(Default)]
pub struct Parker {
    event: AtomicPtr<Event>,
}

impl Parker {
    pub const fn new() -> Self {
        Self {
            event: AtomicPtr::new(ptr::null_mut()),
        }
    }

    pub fn park(&self, deadline: Option<Instant>) -> bool {
        self.park_fast() || self.park_slow(deadline)
    }

    #[inline]
    fn park_fast(&self) -> bool {
        let mut ev = self.event.load(Ordering::Acquire);
        let notified = NonNull::dangling().as_ptr();

        let mut backoff = Backoff::new();
        while !ptr::eq(ev, notified) && backoff.try_yield_now() {
            ev = self.event.load(Ordering::Acquire);
        }

        if !ptr::eq(ev, notified) {
            return false;
        }

        self.park_complete(ev)
    }

    #[cold]
    fn park_slow(&self, deadline: Option<Instant>) -> bool {
        Event::with(|event| {
            let event_ptr = NonNull::from(&*event).as_ptr();
            let notified = NonNull::dangling().as_ptr();
            assert!(!ptr::eq(event_ptr, notified));

            if let Err(ev) = self.event.compare_exchange(
                ptr::null_mut(),
                event_ptr,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                return self.park_complete(ev);
            }

            if !event.wait(deadline) {
                match self.event.compare_exchange(
                    event_ptr,
                    ptr::null_mut(),
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return false,
                    Err(_) => assert!(event.wait(None)),
                }
            }

            let ev = self.event.load(Ordering::Acquire);
            self.park_complete(ev)
        })
    }

    fn park_complete(&self, ev: *mut Event) -> bool {
        assert!(ptr::eq(ev, NonNull::dangling().as_ptr()));
        self.event.store(ptr::null_mut(), Ordering::Relaxed);
        true
    }

    pub unsafe fn unpark(&self) {
        self.event
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |ev| {
                let notified = NonNull::dangling().as_ptr();
                if ptr::eq(ev, notified) {
                    return None;
                }

                Some(notified)
            })
            .map(|ev| Pin::new_unchecked(&*ev).set())
            .unwrap_or(())
    }
}
