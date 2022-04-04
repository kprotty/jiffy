use crate::utils::{CachePadded, Parker, StrictProvenance};
use std::{
    cell::{Cell, UnsafeCell},
    marker::PhantomPinned,
    mem::MaybeUninit,
    pin::Pin,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, Ordering},
};

fn as_mut_ptr<T>(p: Option<NonNull<T>>) -> *mut T {
    p.map(|p| p.as_ptr()).unwrap_or(ptr::null_mut())
}

const IS_RECEIVER: usize = 0b01;
const IS_DISCONNECTED: usize = 0b10;

#[repr(align(4))]
struct Waiter<T> {
    prev: Cell<Option<NonNull<Self>>>,
    next: Cell<Option<NonNull<Self>>>,
    parker: Parker,
    disconnected: Cell<bool>,
    value: UnsafeCell<MaybeUninit<T>>,
    _pinned: PhantomPinned,
}

impl<T> Waiter<T> {
    fn with<F>(f: impl FnOnce(Pin<&Self>) -> F) -> F {
        let waiter = Self {
            prev: Cell::new(None),
            next: Cell::new(None),
            parker: Parker::new(),
            disconnected: Cell::new(false),
            value: UnsafeCell::new(MaybeUninit::uninit()),
            _pinned: PhantomPinned,
        };

        f(unsafe { Pin::new_unchecked(&waiter) })
    }
}

pub struct Queue<T> {
    state: CachePadded<AtomicPtr<Waiter<T>>>,
    consumed: CachePadded<Cell<Option<NonNull<Waiter<T>>>>>,
}

unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Send> Sync for Queue<T> {}

impl<T> Queue<T> {
    pub const fn new() -> Self {
        Self {
            state: CachePadded(AtomicPtr::new(ptr::null_mut())),
            consumed: CachePadded(Cell::new(None)),
        }
    }

    pub fn try_push(&self, value: T) -> Result<(), Result<T, T>> {
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            if state.addr() & IS_DISCONNECTED != 0 {
                return Err(Err(value));
            }

            if state.addr() & IS_RECEIVER == 0 {
                return Err(Ok(value));
            }

            if let Err(e) = self.state.compare_exchange_weak(
                state,
                ptr::null_mut(),
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                state = e;
                continue;
            }

            return Ok(unsafe {
                let receiver = state.map_addr(|addr| addr & !IS_RECEIVER);
                (*receiver).value.get().write(MaybeUninit::new(value));
                (*receiver).parker.unpark();
            });
        }
    }

    pub fn push(&self, value: T) -> Result<(), T> {
        Waiter::with(|waiter| unsafe {
            waiter.value.get().write(MaybeUninit::new(value));
            waiter.prev.set(None);

            let mut state = self.state.load(Ordering::Relaxed);
            loop {
                if state.addr() & IS_DISCONNECTED != 0 {
                    return Err(waiter.value.get().read().assume_init());
                }

                if state.addr() & IS_RECEIVER != 0 {
                    if let Err(e) = self.state.compare_exchange_weak(
                        state,
                        ptr::null_mut(),
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    ) {
                        state = e;
                        continue;
                    }

                    return Ok({
                        let receiver = state.map_addr(|addr| addr & !IS_RECEIVER);
                        (*receiver).value.get().write(waiter.value.get().read());
                        (*receiver).parker.unpark();
                    });
                }

                waiter.next.set(NonNull::new(state));
                if let Err(e) = self.state.compare_exchange_weak(
                    state,
                    NonNull::from(&*waiter).as_ptr(),
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    state = e;
                    continue;
                }

                assert!(waiter.parker.park(None));

                return if waiter.disconnected.get() {
                    Err(waiter.value.get().read().assume_init())
                } else {
                    Ok(())
                };
            }
        })
    }

    pub unsafe fn try_pop(&self) -> Result<Option<T>, ()> {
        'try_pop: loop {
            let tail = as_mut_ptr(self.consumed.get());
            if !tail.is_null() {
                let value = (*tail).value.get().read().assume_init();
                self.consumed.set((*tail).prev.get());
                return Ok(Some(value));
            }

            let mut state = self.state.load(Ordering::Relaxed);
            if state.addr() & IS_DISCONNECTED != 0 {
                return Err(());
            }
            if state.is_null() {
                return Ok(None);
            }

            state = self.state.swap(ptr::null_mut(), Ordering::Acquire);
            assert_eq!(state.addr() & IS_DISCONNECTED, 0);
            assert_eq!(state.addr() & IS_RECEIVER, 0);
            assert!(!state.is_null());

            let head = state;
            assert!((*head).prev.get().is_none());

            let mut tail = head;
            loop {
                let next = as_mut_ptr((*tail).next.get());
                if next.is_null() {
                    self.consumed.set(NonNull::new(tail));
                    continue 'try_pop;
                }

                (*next).prev.set(NonNull::new(tail));
                tail = next;
            }
        }
    }

    pub unsafe fn pop(&self) -> Result<T, ()> {
        if let Some(value) = self.try_pop()? {
            return Ok(value);
        }

        Waiter::with(|waiter| {
            if let Err(_) = self.state.compare_exchange(
                ptr::null_mut(),
                NonNull::from(&*waiter).as_ptr(),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                return Ok(self.try_pop()?.unwrap());
            }

            assert!(waiter.parker.park(None));

            if waiter.disconnected.get() {
                Err(())
            } else {
                Ok(waiter.value.get().read().assume_init())
            }
        })
    }

    pub unsafe fn disconnect(&self, is_sender: bool) {
        let disconnected = ptr::null_mut::<Waiter<T>>().with_addr(IS_DISCONNECTED);
        let mut state = self.state.load(Ordering::Relaxed);

        if ptr::eq(state, disconnected) {
            return;
        } else {
            state = self.state.swap(disconnected, Ordering::AcqRel);
        }

        let mut waiter = state.map_addr(|addr| addr & !IS_RECEIVER);
        while !waiter.is_null() {
            let next = as_mut_ptr((*waiter).next.get());
            (*waiter).disconnected.set(true);
            (*waiter).parker.unpark();
            waiter = next;
        }

        if !is_sender {
            let mut waiter = as_mut_ptr(self.consumed.get());
            while !waiter.is_null() {
                let prev = as_mut_ptr((*waiter).prev.get());
                (*waiter).disconnected.set(true);
                (*waiter).parker.unpark();
                waiter = prev;
            }
        }
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {}
}
