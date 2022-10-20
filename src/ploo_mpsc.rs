use crate::utils::{CachePadded, FetchAddPtr};
use std::{
    cell::UnsafeCell,
    mem::{drop, MaybeUninit},
    ptr::{null_mut, NonNull},
    sync::atomic::{AtomicPtr, AtomicBool, AtomicIsize, Ordering},
};

const BLOCK_SIZE: usize = 1024;
const BLOCK_ALIGN: usize = 4096;
const BLOCK_MASK: usize = !(BLOCK_ALIGN - 1);

#[repr(align(4096))]
struct Block<T> {
    values: [UnsafeCell<MaybeUninit<T>>; BLOCK_SIZE],
    stored: [AtomicBool; BLOCK_SIZE],
    next: AtomicPtr<Self>,
    pending: AtomicIsize,
}

impl<T> Block<T> {
    const EMPTY_STORED: AtomicBool = AtomicBool::new(false);
    const EMPTY_VALUE: UnsafeCell<MaybeUninit<T>> = UnsafeCell::new(MaybeUninit::uninit());
    const EMPTY: Self = Self {
        values: [Self::EMPTY_VALUE; BLOCK_SIZE],
        stored: [Self::EMPTY_STORED; BLOCK_SIZE],
        pending: AtomicIsize::new(0),
        next: AtomicPtr::new(null_mut()),
    };

    unsafe fn unref(block: *mut Self, count: isize) {
        let pending = (*block).pending.fetch_add(count, Ordering::AcqRel);
        if pending + count == 0 {
            drop(Box::from_raw(block));
        }
    }
}

pub struct Queue<T> {
    head: CachePadded<AtomicPtr<Block<T>>>,
    tail: CachePadded<AtomicPtr<Block<T>>>,
}

impl<T> Queue<T> {
    pub const EMPTY: Self = Self {
        head: CachePadded(AtomicPtr::new(null_mut())),
        tail: CachePadded(AtomicPtr::new(null_mut())),
    };

    pub fn send(&self, value: T) {
        unsafe {
            let mut new_block: *mut Block<T> = null_mut();
            loop {
                let mut tail = self.tail.fetch_add(1, Ordering::Acquire);
                let block = tail.map_addr(|addr| addr & BLOCK_MASK);
                let index = tail.addr() & !BLOCK_MASK;

                if !block.is_null() && index < BLOCK_SIZE {
                    (*block).values[index].get().write(MaybeUninit::new(value));
                    (*block).stored[index].store(true, Ordering::Release);

                    if !new_block.is_null() {
                        drop(Box::from_raw(new_block));
                    }

                    return;
                }

                let prev_link = NonNull::new(block)
                    .map(|block| NonNull::from(&block.as_ref().next))
                    .unwrap_or(NonNull::from(&*self.head));

                let mut next_block = prev_link.as_ref().load(Ordering::Acquire);
                if next_block.is_null() {
                    if new_block.is_null() {
                        new_block = Box::into_raw(Box::new(Block::EMPTY));
                    }

                    next_block = new_block;
                    match prev_link.as_ref().compare_exchange(
                        null_mut(),
                        next_block,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => new_block = null_mut(),
                        Err(e) => next_block = e,
                    }
                }

                tail = self.tail.load(Ordering::Relaxed);
                loop {
                    let current_block = tail.map_addr(|addr| addr & BLOCK_MASK);
                    let current_index = tail.addr() & !BLOCK_MASK;

                    if current_block != block {
                        if !block.is_null() {
                            Block::unref(block, 1);
                        }
                        
                        std::hint::spin_loop();
                        break;
                    }
                    
                    if let Err(e) = self.tail.compare_exchange_weak(
                        tail,
                        next_block.map_addr(|addr| addr | 1),
                        Ordering::Release,
                        Ordering::Relaxed,
                    ) {
                        tail = e;
                        continue;
                    }

                    (*next_block).values[0].get().write(MaybeUninit::new(value));
                    (*next_block).stored[0].store(true, Ordering::Release);

                    if !new_block.is_null() && new_block != next_block {
                        drop(Box::from_raw(new_block));
                    }
                    
                    let mut pending = 1isize;
                    let mut unref_block = next_block;

                    if !block.is_null() {
                        assert!(current_index > BLOCK_SIZE);
                        pending = (current_index - BLOCK_SIZE).try_into().unwrap();
                        unref_block = block;
                    }

                    Block::unref(unref_block, -pending);
                    return;
                }
            }
        }
    }

    pub unsafe fn try_recv(&self) -> Option<T> {
        let head = self.head.load(Ordering::Acquire);
        let mut block = head.map_addr(|addr| addr & BLOCK_MASK);
        let mut index = head.addr() & !BLOCK_MASK;

        if block.is_null() {
            return None;
        }
        
        assert!(index <= BLOCK_SIZE);
        if index == BLOCK_SIZE {
            let next_block = (*block).next.load(Ordering::Acquire);
            if next_block.is_null() {
                return None;
            }

            Block::unref(block, 1);
            self.head.store(next_block, Ordering::Relaxed);

            block = next_block;
            index = 0;
        }

        assert!(index < BLOCK_SIZE);
        if !(*block).stored[index].load(Ordering::Acquire) {
            return None;
        }

        self.head.store(block.map_addr(|addr| addr | (index + 1)), Ordering::Relaxed);
        return Some((*block).values[index].get().read().assume_init());
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        let head = self.head.load(Ordering::Acquire);
        let mut block = head.map_addr(|addr| addr & BLOCK_MASK);
        let mut index = head.addr() & !BLOCK_MASK;

        while !block.is_null() {
            unsafe {
                while index < BLOCK_SIZE && (*block).stored[index].load(Ordering::Acquire) {
                    drop((*block).values[index].get().read().assume_init());
                    index += 1;
                }

                let next_block = (*block).next.load(Ordering::Acquire);
                drop(Box::from_raw(block));

                block = next_block;
                index = 0;
            }
        }
    }
}