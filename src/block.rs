use crate::utils::{CachePadded, StrictProvenance};
use std::{
    cell::{Cell, UnsafeCell},
    mem::{drop, MaybeUninit},
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

#[derive(Default)]
struct Backoff(usize);

impl Backoff {
    fn yield_now(&mut self) {
        if cfg!(not(any(target_arch = "x86", target_arch = "x86_64"))) {
            return std::thread::yield_now();
        }
        
        self.0 = (self.0.max(1) * 2).min(1024);
        for _ in 0..self.0 {
            std::hint::spin_loop();
        }
    }
}

struct Value<T>(UnsafeCell<MaybeUninit<T>>);

impl<T> Value<T> {
    const EMPTY: Self = Self(UnsafeCell::new(MaybeUninit::uninit()));
}

const BLOCK_ALIGN: usize = 32;
const BLOCK_SIZE: usize = BLOCK_ALIGN;

#[repr(align(32))]
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
        unsafe {
            let mut backoff = Backoff::default();
            let mut allocated_block = ptr::null_mut::<Block<T>>();

            loop {
                let block_and_index = self.producer.block_and_index.load(Ordering::Relaxed);
                let mut block = block_and_index.map_addr(|addr| addr & !(BLOCK_ALIGN - 1));
                let index = block_and_index.addr() & (BLOCK_ALIGN - 1);

                let mut new_block = block;
                let new_index = (index + 1) % BLOCK_SIZE;

                if new_block.is_null() || new_index == 0 {
                    if allocated_block.is_null() {
                        allocated_block = Box::into_raw(Box::new(Block::EMPTY));
                    }
                    new_block = allocated_block;
                }

                if let Err(_) = self.producer.block_and_index.compare_exchange_weak(
                    block_and_index,
                    new_block.map_addr(|addr| addr | new_index),
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    backoff.yield_now();
                    continue;
                }

                if block != new_block {
                    let prev_link = match NonNull::new(block) {
                        Some(prev) => NonNull::from(&prev.as_ref().next),
                        None => NonNull::from(&self.consumer.block),
                    };
                    prev_link.as_ref().store(new_block, Ordering::Release);
                }

                if block.is_null() {
                    block = new_block;
                }

                (*block).values.get_unchecked(index).0.get().write(MaybeUninit::new(value));
                (*block).stored.get_unchecked(index).store(true, Ordering::Release);
                
                if !allocated_block.is_null() && (new_block != allocated_block) {
                    drop(Box::from_raw(allocated_block));
                }

                return;
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
            self.consumer.block.store(next_block, Ordering::Relaxed);

            index = 0;
            self.consumer.index.set(0);
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