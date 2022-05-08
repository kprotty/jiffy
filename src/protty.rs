use crate::utils::{CachePadded, FetchAddPtr, StrictProvenance};
use std::{
    cell::{Cell, UnsafeCell},
    mem::{drop, MaybeUninit},
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

struct Value<T>(UnsafeCell<MaybeUninit<T>>);

impl<T> Value<T> {
    const EMPTY: Self = Self(UnsafeCell::new(MaybeUninit::uninit()));
}

// max sender threads = BLOCK_ALIGN - BLOCK_SIZE - 1
// thats how much the fetch_add() in push() is allowed to bump until it overflows into the block ptr.
const BLOCK_ALIGN: usize = 4096;
const BLOCK_SIZE: usize = 1024;

#[repr(align(4096))]
struct Block<T> {
    values: [Value<T>; BLOCK_SIZE],
    stored: [AtomicBool; BLOCK_SIZE],
    next: AtomicPtr<Self>,
}

impl<T> Block<T> {
    const UNSTORED: AtomicBool = AtomicBool::new(false);
    const EMPTY: Self = Self {
        values: [Value::<T>::EMPTY; BLOCK_SIZE],
        stored: [Self::UNSTORED; BLOCK_SIZE],
        next: AtomicPtr::new(ptr::null_mut()),
    };
}

struct Producer<T> {
    block_and_index: AtomicPtr<Block<T>>,
}

struct Consumer<T> {
    block: AtomicPtr<Block<T>>,
    index: Cell<usize>,
}

pub struct Queue<T> {
    producer: CachePadded<Producer<T>>,
    consumer: CachePadded<Consumer<T>>,
}

unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Send> Sync for Queue<T> {}

impl<T> Queue<T> {
    pub const EMPTY: Self = Self {
        producer: CachePadded(Producer {
            block_and_index: AtomicPtr::new(ptr::null_mut()),
        }),
        consumer: CachePadded(Consumer {
            block: AtomicPtr::new(ptr::null_mut()),
            index: Cell::new(0),
        }),
    };

    pub fn send(&self, value: T) {
        let mut allocated_block = ptr::null_mut::<Block<T>>();
        loop {
            let mut block_and_index = self
                .producer
                .block_and_index
                .fetch_add(1, Ordering::Acquire);

            let mut block = block_and_index.map_addr(|addr| addr & !(BLOCK_ALIGN - 1));
            let mut index = block_and_index.addr() & (BLOCK_ALIGN - 1);

            if !block.is_null() && index < BLOCK_SIZE {
                return unsafe {
                    (*block).values.get_unchecked(index).0.get().write(MaybeUninit::new(value));
                    (*block).stored.get_unchecked(index).store(true, Ordering::Release);

                    if !allocated_block.is_null() {
                        drop(Box::from_raw(allocated_block));
                    }
                };
            }

            if allocated_block.is_null() {
                allocated_block = Box::into_raw(Box::new(Block::EMPTY));

                unsafe {
                    block = allocated_block;
                    (*block).values.get_unchecked(0).0.get().write(MaybeUninit::new(ptr::read(&value)));
                    (*block).stored.get_unchecked(0).store(true, Ordering::Release);
                }
            }

            loop {
                block = block_and_index.map_addr(|addr| addr & !(BLOCK_ALIGN - 1));
                if block == allocated_block {
                    break; // producer changed, consumer read all; dealloced, then our alloc gave it back
                }

                index = block_and_index.addr() & (BLOCK_ALIGN - 1);
                if !block.is_null() && index < BLOCK_SIZE {
                    break;
                }

                if let Err(e) = self.producer.block_and_index.compare_exchange_weak(
                    block_and_index,
                    allocated_block.map_addr(|addr| addr | 1),
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    block_and_index = e;
                    continue;
                }

                return unsafe {
                    let prev_link = match NonNull::new(block) {
                        Some(prev) => NonNull::from(&prev.as_ref().next),
                        None => NonNull::from(&self.consumer.block),
                    };

                    let next_block = allocated_block;
                    prev_link.as_ref().store(next_block, Ordering::Release);
                };
            }
        }
    }

    pub unsafe fn try_recv(&self) -> Option<T> {
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

            self.consumer.block.store(block, Ordering::Relaxed);
            self.consumer.index.set(index);
        }

        let stored = (*block).stored.get_unchecked(index);
        if !stored.load(Ordering::Acquire) {
            return None;
        }

        let slot = (*block).values.get_unchecked(index);
        self.consumer.index.set(index + 1);
        Some(slot.0.get().read().assume_init())
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        unsafe {
            while let Some(value) = self.try_recv() {
                drop(value);
            }

            let block = self.consumer.block.load(Ordering::Relaxed);
            if !block.is_null() {
                drop(Box::from_raw(block));
            }
        }
    }
}