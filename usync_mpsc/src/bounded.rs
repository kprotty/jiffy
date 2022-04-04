use crate::utils::{Backoff, CachePadded};
use std::{
    alloc::{alloc, dealloc, Layout},
    cell::{Cell, UnsafeCell},
    mem::{drop, MaybeUninit},
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
};

struct Slot<T> {
    value: UnsafeCell<MaybeUninit<T>>,
    stored: AtomicBool,
}

impl<T> Slot<T> {
    const EMPTY: Self = Self {
        value: UnsafeCell::new(MaybeUninit::uninit()),
        stored: AtomicBool::new(false),
    };
}

pub struct Queue<T> {
    producer: CachePadded<AtomicUsize>,
    consumer: CachePadded<Cell<usize>>,
    slots: AtomicPtr<Slot<T>>,
    mask: usize,
}

unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Send> Sync for Queue<T> {}

impl<T> Queue<T> {
    pub const fn new(capacity: usize) -> Self {
        Self {
            producer: CachePadded(AtomicUsize::new(0)),
            consumer: CachePadded(Cell::new(0)),
            slots: AtomicPtr::new(ptr::null_mut()),
            mask: capacity.next_power_of_two() - 1,
        }
    }

    pub fn try_push(&self, value: T) -> Result<(), T> {
        unsafe {
            let mask = self.mask;

            let mut slots = self.slots.load(Ordering::Acquire);
            if slots.is_null() {
                let layout = Layout::array::<Slot<T>>(mask + 1).unwrap();
                slots = alloc(layout).cast();

                if let Err(new_slots) = self.slots.compare_exchange(
                    ptr::null_mut(),
                    slots,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    dealloc(slots.cast(), layout);
                    slots = new_slots;
                }
            }

            let mut backoff = Backoff::new();
            loop {
                let tail = self.producer.load(Ordering::Acquire);
                let slot = &*slots.add(tail & mask);

                if slot.stored.load(Ordering::Acquire) {
                    if tail == self.producer.load(Ordering::Relaxed) {
                        return Err(value);
                    }

                    std::hint::spin_loop();
                    continue;
                }

                if let Err(_) = self.producer.compare_exchange(
                    tail,
                    tail.wrapping_add(1),
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    backoff.yield_now();
                    continue;
                }

                return Ok({
                    slot.value.get().write(MaybeUninit::new(value));
                    slot.stored.store(true, Ordering::Release);
                });
            }
        }
    }

    pub unsafe fn try_pop(&self) -> Option<T> {
        let slots = self.slots.load(Ordering::Acquire);
        if slots.is_null() {
            return None;
        }

        let head = self.consumer.get();
        let slot = &*slots.add(head & self.mask);

        if !slot.stored.load(Ordering::Acquire) {
            return None;
        }

        let value = slot.value.get().read().assume_init();
        self.consumer.set(head.wrapping_add(1));

        slot.stored.store(false, Ordering::Release);
        Some(value)
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        unsafe {
            while let Some(value) = self.try_pop() {
                drop(value);
            }

            let slots = self.slots.load(Ordering::Acquire);
            if !slots.is_null() {
                let layout = Layout::array::<Slot<T>>(self.mask + 1).unwrap();
                dealloc(slots.cast(), layout);
            }
        }
    }
}
