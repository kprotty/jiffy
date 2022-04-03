use crate::{shared::StrictProvenance, utils::CachePadded};
use std::{
    cell::UnsafeCell,
    hint::spin_loop,
    mem::{drop, MaybeUninit},
    ptr,
    sync::atomic::{AtomicPtr, AtomicU8, Ordering},
    thread,
};

#[derive(Default)]
struct Backoff {
    counter: usize,
}

impl Backoff {
    fn yield_now(&mut self) {
        self.counter = self.counter.wrapping_add(1);

        if self.counter <= 3 {
            (0..(1 << self.counter)).for_each(|_| spin_loop());
            return;
        }

        if cfg!(all(
            windows,
            any(target_arch = "x86", target_arch = "x86_64")
        )) {
            (0..(1 << self.counter.min(10))).for_each(|_| spin_loop());
            return;
        }

        thread::yield_now();
    }
}

enum Read<T> {
    Empty,
    Consumed(T),
    AlreadyConsumed,
}

const EMPTY: u8 = 0;
const STORED: u8 = 1;
const CONSUMED: u8 = 2;

struct Slot<T> {
    value: UnsafeCell<MaybeUninit<T>>,
    state: AtomicU8,
}

impl<T> Slot<T> {
    const INIT: Self = Self {
        value: UnsafeCell::new(MaybeUninit::uninit()),
        state: AtomicU8::new(EMPTY),
    };

    unsafe fn write(&self, value: T) {
        assert_eq!(self.state.load(Ordering::Relaxed), EMPTY);
        self.value.get().write(MaybeUninit::new(value));
        self.state.store(STORED, Ordering::Release);
    }

    unsafe fn read(&self) -> Read<T> {
        match self.state.load(Ordering::Acquire) {
            EMPTY => Read::Empty,
            STORED => {
                self.state.store(CONSUMED, Ordering::Relaxed);
                Read::Consumed(self.value.get().read().assume_init())
            }
            state => {
                assert_eq!(state, CONSUMED);
                Read::AlreadyConsumed
            }
        }
    }
}

const BLOCK_SIZE: usize = 32;
const BLOCK_MASK: usize = BLOCK_SIZE - 1;

#[repr(align(32))]
struct Block<T> {
    slots: [Slot<T>; BLOCK_SIZE],
    next: AtomicPtr<Self>,
}

impl<T> Block<T> {
    const fn new() -> Self {
        Self {
            slots: [Slot::<T>::INIT; BLOCK_SIZE],
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

#[derive(Default)]
pub struct MpscQueue<T> {
    producer: CachePadded<AtomicPtr<Block<T>>>,
    consumer: CachePadded<AtomicPtr<Block<T>>>,
}

impl<T> MpscQueue<T> {
    pub const fn new() -> Self {
        Self {
            producer: CachePadded(AtomicPtr::new(ptr::null_mut())),
            consumer: CachePadded(AtomicPtr::new(ptr::null_mut())),
        }
    }

    pub fn push(&self, value: T) {
        unsafe {
            let mut backoff = Backoff::default();
            let mut next_block = ptr::null_mut::<Block<T>>();

            loop {
                let producer = self.producer.load(Ordering::Relaxed);
                let mut block = producer.map_addr(|addr| addr & !BLOCK_MASK);
                let index = producer.addr() & BLOCK_MASK;

                let mut new_block = block;
                let new_index = (index + 1) & BLOCK_MASK;

                if block.is_null() || new_index == 0 {
                    if next_block.is_null() {
                        next_block = Box::into_raw(Box::new(Block::new()));
                    }
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

                if block.is_null() {
                    block = new_block;
                    self.consumer.store(new_block, Ordering::Release);
                } else if new_index == 0 {
                    assert!(!ptr::eq(block, new_block));
                    (*block).next.store(new_block, Ordering::Release);
                } else if !next_block.is_null() {
                    assert!(ptr::eq(block, new_block));
                    drop(Box::from_raw(next_block));
                }

                (*block).slots.get(index).unwrap().write(value);
                return;
            }
        }
    }

    pub unsafe fn pop(&self) -> Option<T> {
        let consumer = self.consumer.load(Ordering::Acquire);
        let mut block = consumer.map_addr(|addr| addr & !BLOCK_MASK);
        let mut index = consumer.addr() & BLOCK_MASK;

        while !block.is_null() {
            match (*block).slots.get(index).unwrap().read() {
                Read::Empty => break,
                Read::AlreadyConsumed => assert_eq!(index, 0),
                Read::Consumed(value) => {
                    index = (index + 1) & BLOCK_MASK;
                    block = block.map_addr(|addr| addr | index);
                    self.consumer.store(block, Ordering::Relaxed);
                    return Some(value);
                }
            }

            let next_block = (*block).next.load(Ordering::Acquire);
            if next_block.is_null() {
                break;
            }

            self.consumer.store(next_block, Ordering::Relaxed);
            drop(Box::from_raw(block));
            block = next_block;
        }

        None
    }
}

impl<T> Drop for MpscQueue<T> {
    fn drop(&mut self) {
        unsafe {
            let consumer = self.consumer.load(Ordering::Acquire);
            let mut block = consumer.map_addr(|addr| addr & !BLOCK_MASK);
            let mut index = consumer.addr() & BLOCK_MASK;

            while !block.is_null() {
                if let Read::Consumed(value) = (*block).slots.get(index).unwrap().read() {
                    index = (index + 1) & BLOCK_MASK;
                    drop(value);
                    continue;
                }

                let next = (*block).next.load(Ordering::Acquire);
                drop(Box::from_raw(block));
                block = next;
            }
        }
    }
}
