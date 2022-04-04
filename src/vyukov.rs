use crate::utils::CachePadded;
use std::{
    cell::{Cell, UnsafeCell},
    hint::spin_loop,
    mem::{drop, MaybeUninit},
    sync::atomic::{AtomicUsize, AtomicBool, Ordering},
    thread,
};

#[derive(Default)]
struct Backoff(usize);

impl Backoff {
    fn yield_now(&mut self) {
        if cfg!(windows) || self.0 <= 6 {
            (0..(1 << self.0)).for_each(|_| spin_loop());
            self.0 = self.0.wrapping_add(1);
            return;
        }

        thread::yield_now();
    }
}

struct Slot<T> {
    stored: AtomicBool,
    value: UnsafeCell<MaybeUninit<T>>,
}

pub struct Queue<T> {
    head: CachePadded<Cell<usize>>,
    tail: CachePadded<AtomicUsize>,
    slots: Box<[Slot<T>]>,
}

unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Send> Sync for Queue<T> {}

impl<T> Queue<T> {
    pub fn new(mut capacity: usize) -> Self {
        Self {
            head: CachePadded(Cell::new(0)),
            tail: CachePadded(AtomicUsize::new(0)),
            slots: (0..capacity.next_power_of_two())
                .map(|_| Slot {
                    stored: AtomicBool::new(false),
                    value: UnsafeCell::new(MaybeUninit::uninit()),
                })
                .collect(),
        }
    }

    pub fn push(&self, item: T) -> Result<(), T> {
        let mut backoff = Backoff::default();
        let mask = self.slots.len() - 1;

        loop {
            let tail = self.tail.load(Ordering::Relaxed);
            let slot = unsafe { self.slots.get_unchecked(tail & mask) };

            if slot.stored.load(Ordering::Acquire) {
                if self.tail.load(Ordering::Relaxed) == tail {
                    return Err(item);
                }    

                spin_loop();
                continue;
            }

            if let Err(_) = self.tail.compare_exchange(
                tail,
                tail.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                std::thread::yield_now();
                continue;
            }

            return Ok(unsafe {
                slot.value.get().write(MaybeUninit::new(item));
                slot.stored.store(true, Ordering::Release);
            });
        }
    }

    pub unsafe fn pop(&self) -> Option<T> {
        let head = self.head.get();
        let mask = self.slots.len() - 1;

        let slot = self.slots.get_unchecked(head & mask);
        if !slot.stored.load(Ordering::Acquire) {
            return None;
        }

        let value = slot.value.get().read().assume_init();
        slot.stored.store(false, Ordering::Release);

        self.head.set(head.wrapping_add(1));
        Some(value)
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        while let Some(value) = unsafe { self.pop() } {
            drop(value);
        }
    }
}
