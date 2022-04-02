use crate::{
    shared::{pinned, Backoff, StrictProvenance, Waiter},
    utils::CachePadded,
};
use std::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

#[derive(Default)]
struct WaitList {
    stack: AtomicPtr<Waiter>,
}

impl WaitList {
    const fn new() -> Self {
        Self {
            stack: AtomicPtr::new(ptr::null_mut()),
        }
    }

    #[inline]
    fn should_park(&self) -> bool {
        let stack = self.stack.load(Ordering::Relaxed);
        !stack.is_null()
    }

    #[cold]
    fn park_while(&self, should_wait: impl FnOnce() -> bool) {
        pinned::<Waiter, _, _>(|waiter| {
            let _ = self
                .stack
                .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |stack| {
                    waiter.next.set(NonNull::new(stack));
                    Some(NonNull::from(&*waiter).as_ptr())
                });

            if !should_wait() {
                self.unpark_all();
            }

            waiter.park();
        })
    }

    #[cold]
    fn unpark_all(&self) {
        unsafe {
            let mut stack = self.stack.swap(ptr::null_mut(), Ordering::AcqRel);
            while !stack.is_null() {
                let waiter = stack;
                let next = (*waiter).next.get();

                stack = next.map(|p| p.as_ptr()).unwrap_or(ptr::null_mut());
                (*waiter).unpark();
            }
        }
    }
}

struct Slot<T> {
    active: AtomicBool,
    value: UnsafeCell<MaybeUninit<T>>,
}

impl<T> Slot<T> {
    const EMPTY: Self = Self {
        active: AtomicBool::new(false),
        value: UnsafeCell::new(MaybeUninit::uninit()),
    };

    unsafe fn write(&self, value: T) {
        self.value.get().write(MaybeUninit::new(value));
        assert!(!self.active.load(Ordering::Relaxed));
        self.active.store(true, Ordering::Release);
    }

    unsafe fn read(&self) -> Option<T> {
        if self.active.load(Ordering::Acquire) {
            Some(self.value.get().read().assume_init())
        } else {
            None
        }
    }
}

const LAP: usize = 32;
const CAPACITY: usize = LAP - 1;

#[repr(align(32))]
struct Block<T> {
    slots: [Slot<T>; CAPACITY],
    next: AtomicPtr<Self>,
}

impl<T> Block<T> {
    const fn new() -> Self {
        Self {
            slots: [Slot::<T>::EMPTY; CAPACITY],
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

#[derive(Default)]
pub struct MpscQueue<T> {
    head: CachePadded<AtomicPtr<Block<T>>>,
    tail: CachePadded<AtomicPtr<Block<T>>>,
    waiters: CachePadded<WaitList>,
}

impl<T> MpscQueue<T> {
    pub const fn new() -> Self {
        Self {
            head: CachePadded(AtomicPtr::new(ptr::null_mut())),
            tail: CachePadded(AtomicPtr::new(ptr::null_mut())),
            waiters: CachePadded(WaitList::new()),
        }
    }

    pub fn push(&self, value: T) {
        let mut next_block = None;
        let mut backoff = Backoff::default();

        loop {
            let mut block = self.tail.load(Ordering::Acquire);
            if block.is_null() {
                let new_block = Box::into_raw(Box::new(Block::new()));
                assert_eq!(new_block.addr() & CAPACITY, 0);

                if let Err(_) = self.tail.compare_exchange(
                    ptr::null_mut(),
                    new_block,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    drop(unsafe { Box::from_raw(new_block) });
                    continue;
                }

                block = new_block;
                self.head.store(block, Ordering::Release);
            }

            let index = block.addr() & CAPACITY;
            if index == CAPACITY {
                if self.waiters.should_park() || !backoff.try_yield_now() {
                    self.waiters.park_while(|| {
                        let current_block = self.tail.load(Ordering::Acquire);
                        block.addr() == current_block.addr()
                    });
                }

                continue;
            }

            let new_index = index + 1;
            if new_index == CAPACITY && next_block.is_none() {
                next_block = Some(Box::new(Block::new()));
            }

            if let Err(_) = self.tail.compare_exchange(
                block,
                block.map_addr(|ptr| (ptr & !CAPACITY) | new_index),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                backoff.yield_now();
                continue;
            }

            unsafe {
                let block = block.map_addr(|ptr| ptr & !CAPACITY);
                assert!(!block.is_null());

                if new_index == CAPACITY {
                    let next_block = Box::into_raw(next_block.unwrap());
                    assert_eq!(next_block.addr() & CAPACITY, 0);

                    self.tail.store(next_block, Ordering::Release);
                    self.waiters.unpark_all();

                    assert!((*block).next.load(Ordering::Relaxed).is_null());
                    (*block).next.store(next_block, Ordering::Release);
                }

                (*block).slots.get(index).unwrap().write(value);
                return;
            }
        }
    }

    pub unsafe fn pop(&self) -> Option<T> {
        let mut block = self.head.load(Ordering::Acquire);
        if block.is_null() {
            return None;
        }

        let mut index = block.addr() & CAPACITY;
        block = block.map_addr(|ptr| ptr & !CAPACITY);

        if index == CAPACITY {
            block = (*block).next.load(Ordering::Acquire);
            index = 0;
        }

        if block.is_null() {
            return None;
        }

        let value = (*block).slots.get(index).unwrap().read();
        if value.is_some() {
            index += 1;
        }

        let with_index = block.map_addr(|ptr| ptr | index);
        self.head.store(with_index, Ordering::Relaxed);
        value
    }
}
