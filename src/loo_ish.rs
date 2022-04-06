use crate::utils::{StrictProvenance, CachePadded, FetchAddPtr};
use std::{
    ptr::{self, NonNull},
    cell::{Cell, UnsafeCell},
    mem::{drop, MaybeUninit},
    sync::atomic::{AtomicPtr, AtomicBool, Ordering},
};

struct Value<T>(UnsafeCell<MaybeUninit<T>>);

impl<T> Value<T> {
    const EMPTY: Self = Self(UnsafeCell::new(MaybeUninit::uninit()));

    unsafe fn write(&self, value: T) {
        self.0.get().write(MaybeUninit::new(value))
    }

    unsafe fn read(&self) -> T {
        self.0.get().read().assume_init()
    }
}

const BLOCK_MASK: usize = 1024 - 1;
const BLOCK_SIZE: usize = 32;

#[repr(align(1024))]
struct Block<T> {
    values: [Value<T>; BLOCK_SIZE],
    stored: [AtomicBool; BLOCK_SIZE],
    next: AtomicPtr<Self>,
}

impl<T> Block<T> {
    const NOT_STORED: AtomicBool = AtomicBool::new(false);
    const EMPTY: Self = Self {
        values: [Value::<T>::EMPTY; BLOCK_SIZE],
        stored: [Self::NOT_STORED; BLOCK_SIZE],
        next: AtomicPtr::new(ptr::null_mut()),
    };
}

pub struct Queue<T> {
    head: CachePadded<AtomicPtr<Block<T>>>,
    tail: CachePadded<AtomicPtr<Block<T>>>,
    next: CachePadded<AtomicPtr<Block<T>>>,
}

impl<T> Queue<T> {
    pub const fn new() -> Self {
        Self {
            head: CachePadded(AtomicPtr::new(ptr::null_mut())),
            tail: CachePadded(AtomicPtr::new(ptr::null_mut())),
            next: CachePadded(AtomicPtr::new(ptr::null_mut())),
        }
    }

    pub fn send(&self, value: T) {
        let mut next_block = ptr::null_mut::<Block<T>>();

        loop {
            let mut tail = self.tail.fetch_add(1, Ordering::Acquire);
            let block = tail.map_addr(|addr| addr & !BLOCK_MASK);
            let index = tail.addr() & BLOCK_MASK;

            if !block.is_null() && index < BLOCK_SIZE {
                unsafe {
                    if !next_block.is_null() {
                        drop(Box::from_raw(next_block));
                    }

                    (*block).values.get_unchecked(index).write(value);
                    (*block).stored.get_unchecked(index).store(true, Ordering::Release);
                    return;
                }
            }

            let mut next = self.next.load(Ordering::Acquire);
            if ptr::eq(next, block) {
                if next_block.is_null() {
                    next_block = Box::into_raw(Box::new(Block::EMPTY));
                }
                
                let new_next = next_block;
                next = match self.next.compare_exchange(
                    next,
                    new_next,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Err(e) => e,
                    Ok(_) => {
                        next_block = ptr::null_mut();
                        new_next
                    }
                };
            }

            tail = self.tail.load(Ordering::Relaxed);
            loop {
                let current_block = tail.map_addr(|addr| addr & !BLOCK_MASK);
                if !ptr::eq(block, current_block) {
                    break;
                }

                if let Err(e) = self.tail.compare_exchange_weak(
                    tail,
                    next.map_addr(|addr| addr | 1),
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    tail = e;
                    continue;
                }

                unsafe {
                    let prev_block_next = match NonNull::new(block) {
                        Some(prev_block) => NonNull::from(&prev_block.as_ref().next),
                        None => NonNull::from(&*self.head),
                    };

                    (*next).values.get_unchecked(0).write(value);
                    (*next).stored.get_unchecked(0).store(true, Ordering::Release);

                    prev_block_next.as_ref().store(next, Ordering::Release);
                    return;
                }
            }
        }
    }

    pub unsafe fn try_recv(&self) -> Option<T> {
        let head = self.head.load(Ordering::Acquire);
        
        let mut block = head.map_addr(|addr| addr & !BLOCK_MASK);
        if block.is_null() {
            return None;
        }

        let mut index = head.addr() & BLOCK_MASK;
        if index == BLOCK_SIZE {
            block = (*block).next.load(Ordering::Acquire);
            if block.is_null() {
                return None;
            }

            index = 0;
            self.head.store(block, Ordering::Relaxed);
        }
        
        if !(*block).stored.get_unchecked(index).load(Ordering::Acquire) {
            return None;
        }

        let new_head = block.map_addr(|addr| addr | (index + 1));
        self.head.store(new_head, Ordering::Relaxed);

        let value = (*block).values.get_unchecked(index).read();
        Some(value)
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        unsafe {
            let head = self.head.load(Ordering::Acquire);
            let mut block = head.map_addr(|addr| addr & !BLOCK_MASK);
            let mut index = head.addr() & BLOCK_MASK;

            while !block.is_null() {
                if !(*block).stored.get_unchecked(index).load(Ordering::Acquire) {
                    break;
                }

                drop((*block).values.get_unchecked(index).read());
                index = (index + 1) % BLOCK_SIZE;

                if index == 0 {
                    let next_block = (*block).next.load(Ordering::Acquire);
                    drop(Box::from_raw(block));
                    block = next_block;
                }
            }

            if !block.is_null() {
                assert!((*block).next.load(Ordering::Relaxed).is_null());
                drop(Box::from_raw(block));
            }
        }
    }
}