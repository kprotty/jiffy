use crate::utils::CachePadded;
use std::{
    cell::{Cell, UnsafeCell},
    hint::spin_loop,
    mem::{drop, MaybeUninit},
    sync::atomic::{AtomicUsize, Ordering},
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
    seq: AtomicUsize,
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
    pub fn new(capacity: usize) -> Self {
        Self {
            head: CachePadded(Cell::new(0)),
            tail: CachePadded(AtomicUsize::new(0)),
            slots: (0..capacity)
                .map(|i| Slot {
                    seq: AtomicUsize::new(i),
                    value: UnsafeCell::new(MaybeUninit::uninit()),
                })
                .collect(),
        }
    }

    pub fn push(&self, item: T) -> Result<(), T> {
        let mut backoff = Backoff::default();
        let mut tail = self.tail.load(Ordering::Relaxed);

        loop {
            let index = tail % self.slots.len();
            let slot = unsafe { self.slots.get_unchecked(index) };

            let seq = slot.seq.load(Ordering::Acquire);
            let diff = (seq as isize).wrapping_sub(tail as isize);
            if diff < 0 {
                return Err(item);
            }

            if diff == 0 {
                let new_tail = tail.wrapping_add(1);
                if let Err(e) =
                    self.tail
                        .compare_exchange(tail, new_tail, Ordering::Acquire, Ordering::Relaxed)
                {
                    backoff.yield_now();
                    tail = e;
                    continue;
                }

                unsafe { slot.value.get().write(MaybeUninit::new(item)) };
                slot.seq.store(new_tail, Ordering::Release);
                return Ok(());
            }

            backoff.yield_now();
            tail = self.tail.load(Ordering::Relaxed);
        }
    }

    pub unsafe fn pop(&self) -> Option<T> {
        let head = self.head.get();
        let new_head = head.wrapping_add(1);

        let index = head % self.slots.len();
        let slot = self.slots.get_unchecked(index);
        let seq = slot.seq.load(Ordering::Acquire);

        let diff = (seq as isize).wrapping_sub(new_head as isize);
        if diff == 0 {
            let value = slot.value.get().read().assume_init();
            let new_seq = head.wrapping_add(self.slots.len());
            slot.seq.store(new_seq, Ordering::Release);

            self.head.set(new_head);
            return Some(value);
        }

        None
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        while let Some(value) = unsafe { self.pop() } {
            drop(value);
        }
    }
}
