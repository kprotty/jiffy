use crate::utils::{Backoff, CachePadded, StrictProvenance};
use std::{
    cell::{Cell, UnsafeCell},
    mem::{drop, MaybeUninit},
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
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

const BLOCK_SIZE: usize = 32;
const BLOCK_MASK: usize = BLOCK_SIZE - 1;

#[repr(align(32))]
struct Block<T> {
    slots: [Slot<T>; BLOCK_SIZE],
    next: AtomicPtr<Self>,
}

impl<T> Block<T> {
    const EMPTY: Self = Self {
        slots: [Slot::<T>::EMPTY; BLOCK_SIZE],
        next: AtomicPtr::new(ptr::null_mut()),
    };
}

struct Consumer<T> {
    block: AtomicPtr<Block<T>>,
    index: Cell<usize>,
}

pub struct Queue<T> {
    producer: CachePadded<AtomicPtr<Block<T>>>,
    consumer: CachePadded<Consumer<T>>,
}

unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Send> Sync for Queue<T> {}

impl<T> Queue<T> {
    pub const fn new() -> Self {
        Self {
            producer: CachePadded(AtomicPtr::new(ptr::null_mut())),
            consumer: CachePadded(Consumer {
                block: AtomicPtr::new(ptr::null_mut()),
                index: Cell::new(0),
            }),
        }
    }

    pub fn push(&self, value: T) {
        let mut backoff = Backoff::new();
        let mut next_block = ptr::null_mut::<Block<T>>();

        loop {
            let producer = self.producer.load(Ordering::Relaxed);
            let mut block = producer.map_addr(|addr| addr & !BLOCK_MASK);
            let index = producer.addr() & BLOCK_MASK;

            let mut new_block = block;
            let mut new_index = (index + 1) & BLOCK_MASK;

            if block.is_null() || new_index == 0 {
                if next_block.is_null() {
                    next_block = Box::into_raw(Box::new(Block::EMPTY));
                }

                debug_assert!(!next_block.is_null());
                new_block = next_block;
            }

            if let Err(_) = self.producer.compare_exchange(
                producer,
                new_block.map_addr(|addr| addr | new_index),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                backoff.yield_now();
                continue;
            }

            return unsafe {
                if block.is_null() {
                    block = new_block;
                    self.consumer.block.store(new_block, Ordering::Release);
                }

                if new_index == 0 {
                    (*block).next.store(new_block, Ordering::Release);
                }

                if !next_block.is_null() && !ptr::eq(block, next_block) {
                    drop(Box::from_raw(next_block));
                }

                let slot = (*block).slots.get_unchecked(index);
                slot.value.get().write(MaybeUninit::new(value));
                slot.stored.store(true, Ordering::Release);
            };
        }
    }

    pub unsafe fn try_pop(&self) -> Option<T> {
        let mut block = self.consumer.block.load(Ordering::Acquire);
        if block.is_null() {
            return None;
        }

        let mut index = self.consumer.index.get();
        if index == BLOCK_SIZE {
            let next_block = (*block).next.load(Ordering::Acquire);
            if next_block.is_null() {
                return None;
            }

            drop(Box::from_raw(block));
            block = next_block;
            index = 0;

            self.consumer.index.set(index);
            self.consumer.block.store(block, Ordering::Relaxed);
        }

        let slot = (*block).slots.get_unchecked(index);
        if !slot.stored.load(Ordering::Acquire) {
            return None;
        }

        self.consumer.index.set(index + 1);
        Some(slot.value.get().read().assume_init())
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        unsafe {
            while let Some(value) = self.try_pop() {
                drop(value);
            }

            let block = self.consumer.block.load(Ordering::Relaxed);
            if !block.is_null() {
                drop(Box::from_raw(block));
            }
        }
    }
}
