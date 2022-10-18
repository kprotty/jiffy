use crate::utils::{CachePadded};
use std::{
    cell::UnsafeCell,
    mem::{drop, MaybeUninit},
    ptr::{null_mut, NonNull},
    sync::atomic::{AtomicPtr, AtomicBool, Ordering},
};

const BLOCK_SIZE: usize = 64;
const BLOCK_ALIGN: usize = BLOCK_SIZE << 1;
const BLOCK_MASK: usize = !(BLOCK_ALIGN - 1);

#[repr(align(128))]
struct Block<T> {
    next: AtomicPtr<Self>,
    stored: [AtomicBool; BLOCK_SIZE],
    values: [UnsafeCell<MaybeUninit<T>>; BLOCK_SIZE],
}

impl<T> Block<T> {
    const EMPTY_STORED: AtomicBool = AtomicBool::new(false);
    const EMPTY_VALUE: UnsafeCell<MaybeUninit<T>> = UnsafeCell::new(MaybeUninit::uninit());
    const EMPTY: Self = Self {
        next: AtomicPtr::new(null_mut()),
        stored: [Self::EMPTY_STORED; BLOCK_SIZE],
        values: [Self::EMPTY_VALUE; BLOCK_SIZE],
    };
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
        let mut backoff = 0;
        let mut new_block: *mut Block<T> = null_mut();
        
        loop {
            let tail = self.tail.load(Ordering::Relaxed);
            let block = tail.map_addr(|addr| addr & BLOCK_MASK);
            let index = tail.addr() & !BLOCK_MASK;
            assert!(index <= BLOCK_SIZE);

            let mut next_block = block;
            let mut next_index = index + 1;

            if block.is_null() || index == BLOCK_SIZE {
                if new_block.is_null() {
                    new_block = Box::into_raw(Box::new(Block::<T>::EMPTY));
                    assert!(!new_block.is_null());
                }

                next_block = new_block;
                next_index = 1;
            }

            if let Err(e) = self.tail.compare_exchange(
                tail,
                next_block.map_addr(|addr| addr | next_index),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                #[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
                {
                    _ = backoff;
                    unsafe { std::arch::asm!("wfe", options(nomem, nostack)) };
                }
                
                #[cfg(not(all(target_vendor = "apple", target_arch = "aarch64")))]
                {
                    backoff = (backoff + 1).min(10);
                    (0..(1 << backoff)).for_each(|_| std::hint::spin_loop());
                }
                
                continue;
            }

            return unsafe {
                let index = index % BLOCK_SIZE;
                (*next_block).values[index].get().write(MaybeUninit::new(value));
                (*next_block).stored[index].store(true, Ordering::Release);

                if block != next_block {
                    let prev_link = match NonNull::new(block) {
                        Some(prev_block) => NonNull::from(&prev_block.as_ref().next),
                        None => NonNull::from(&*self.head),
                    };

                    assert!(prev_link.as_ref().load(Ordering::Relaxed).is_null());
                    prev_link.as_ref().store(next_block, Ordering::Release);
                } else if !new_block.is_null() {
                    drop(Box::from_raw(new_block));
                }
            };
        }
    }

    pub unsafe fn try_recv(&self) -> Option<T> {
        let head = self.head.load(Ordering::Acquire);

        let mut block = head.map_addr(|addr| addr & BLOCK_MASK);
        if block.is_null() {
            return None;
        }

        let mut index = head.addr() & !BLOCK_MASK;
        assert!(index <= BLOCK_SIZE);

        if index == BLOCK_SIZE {
            let next_block = (*block).next.load(Ordering::Acquire);
            if next_block.is_null() {
                return None;
            }

            drop(Box::from_raw(block));
            block = next_block;
            index = 0;

            let head = block.map_addr(|addr| addr | index);
            self.head.store(head, Ordering::Relaxed);
        }

        if (*block).stored[index].load(Ordering::Acquire) {
            let head = block.map_addr(|addr| addr | (index + 1));
            self.head.store(head, Ordering::Relaxed);

            let value = (*block).values[index].get().read().assume_init();
            return Some(value);
        }
        
        None
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        let head = self.head.load(Ordering::Acquire);
        let mut block = head.map_addr(|addr| addr & BLOCK_MASK);
        let mut index = head.addr() & !BLOCK_MASK;

        unsafe {
            while !block.is_null() {
                assert!(index <= BLOCK_SIZE);

                if index < BLOCK_SIZE {
                    if (*block).stored[index].load(Ordering::Acquire) {
                        drop((*block).values[index].get().read().assume_init());
                        index += 1;
                        continue;
                    }
                }

                let next_block = (*block).next.load(Ordering::Acquire);
                drop(Box::from_raw(block));
                block = next_block;
                index = 0;
            }
        }
    }
}