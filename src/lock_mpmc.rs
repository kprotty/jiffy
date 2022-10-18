mod internal {
    use crate::utils::CachePadded;
    use std::{
        sync::Arc,
        cell::Cell,
        mem::{drop, replace},
        ptr::NonNull,
        collections::VecDeque,
        sync::atomic::{AtomicBool, Ordering},
    };

    #[cfg(target_vendor = "apple")]
    fn wait_until(b: &AtomicBool, value: bool, ordering: Ordering) {
        // https://github.com/apple/swift-corelibs-libdispatch/blob/main/src/shims/yield.h#L62-L80
        let mut spins: std::os::raw::c_int = if cfg!(target_arch = "aarch64") { -10 } else  { -1024 };
        loop {
            if spins < 0 {
                #[cfg(target_arch = "aarch64")]
                unsafe { std::arch::asm!("wfe", options(nomem, nostack)); }
                #[cfg(not(target_arch = "aarch64"))]
                std::hint::spin_loop();
            } else {
                extern "C" {
                    fn thread_switch(
                        port: *mut std::os::raw::c_void,
                        option: std::os::raw::c_int,
                        time: u32,
                    ) -> std::os::raw::c_int;
                }
                const SWITCH_OPTION_DEPRESS: std::os::raw::c_int = 1;
                thread_switch(std::ptr::null_mut(), SWITCH_OPTION_DEPRESS, spins);
            }

            if b.load(ordering) == value {
                return;
            } else {
                spins += 1;
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn wait_until(b: &AtomicBool, value: bool, ordering: Ordering) {
        // https://www.1024cores.net/home/lock-free-algorithms/tricks/spinning
        for i in 0.. {
            match () {
                _ if i < 10 => std::hint::spin_loop(),
                _ if i < 12 => std::thread::yield_now(),
                _ if i < 14 => std::thread::sleep(std::time::Duration::from_millis(0)),
                _ if i < 16 => std::thread::sleep(std::time::Duration::from_millis(1)),
                _ => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
            if b.load(ordering) == value {
                return;
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn wait_until(b: &AtomicBool, value: bool, ordering: Ordering) {
        loop {
            std::thread::yield_now();
            if b.load(ordering) == value {
                return;
            }
        }
    }

    #[cfg(not(any(target_vendor = "apple", target_os = "windows", target_os = "linux")))]
    fn wait_until(b: &AtomicBool, value: bool, ordering: Ordering) {
        loop {
            std::thread::yield_now();
            if b.load(ordering) == value {
                return;
            }
        }
    }

    pub struct RawMutex {
        locked: AtomicBool,
    }

    unsafe impl lock_api::RawMutex for RawMutex {
        const INIT: Self = Self {
            locked: AtomicBool::new(false),
        };

        type GuardMarker = lock_api::GuardSend;

        #[inline(always)]
        fn try_lock(&self) -> bool {
            self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok()
        }

        fn lock(&self) {
            if !self.try_lock() {
                loop {
                    wait_until(&self.locked, false, Ordering::Relaxed);
                    if self.try_lock() {
                        break;
                    }
                }
            }
        }

        #[inline(always)]
        unsafe fn unlock(&self) {
            self.locked.store(false, Ordering::Release);
        }
    }

    enum Slot<T> {
        Empty,
        Value(T),
        Disconnected(Option<T>),
    }

    struct Waiter<T> {
        stored: AtomicBool,
        slot: Cell<Slot<T>>,
    }

    pub struct Inner<T> {
        queue: VecDeque<T>,
        capacity: usize,
        senders: usize,
        receivers: usize,
        send_queue: VecDeque<NonNull<Waiter<T>>>,
        recv_queue: VecDeque<NonNull<Waiter<T>>>,
    }

    unsafe impl<T: Send> Send for Inner<T> {}
    
    pub type Mutex<T> = lock_api::Mutex<RawMutex, T>;
    pub type Channel<T> = Arc<Mutex<CachePadded<Inner<T>>>>;

    pub fn channel<T>(capacity: Option<usize>) -> Channel<T> {
        Arc::new(Mutex::new(CachePadded(Inner {
            queue: VecDeque::with_capacity(capacity.unwrap_or(2048)),
            capacity: capacity.unwrap_or(usize::MAX),
            senders: 1,
            receivers: 1,
            send_queue: VecDeque::new(),
            recv_queue: VecDeque::new(),
        })))
    }

    pub fn clone<T>(chan: &Channel<T>, sender: bool) {
        let mut inner = chan.lock();

        if sender {
            assert_ne!(inner.senders, 0);
            inner.senders += 1;
        } else {
            assert_ne!(inner.receivers, 0);
            inner.receivers += 1;
        }
    }

    pub fn disconnect<T>(chan: &Channel<T>, sender: bool) {
        let mut inner = chan.lock();

        let disconnected = if sender {
            inner.senders -= 1;
            inner.senders == 0
        } else {
            inner.receivers -= 1;
            inner.receivers == 0
        };

        if !disconnected {
            return;
        }

        let mut waiters = replace(&mut inner.send_queue, VecDeque::new());
        waiters.append(&mut inner.recv_queue);
        drop(inner);

        for waiter in waiters.into_iter() {
            unsafe {
                match waiter.as_ref().slot.replace(Slot::Disconnected(None)) {
                    Slot::Empty => {},
                    Slot::Disconnected(_) => unreachable!(),
                    Slot::Value(value) => waiter.as_ref().slot.set(Slot::Disconnected(Some(value))),
                }
                waiter.as_ref().stored.store(true, Ordering::Release);
            }
        }
    }

    pub fn send<T>(chan: &Channel<T>, value: T, block: bool) -> Result<(), Result<T, T>> {
        let mut inner = chan.lock();

        if let Some(waiter) = inner.recv_queue.pop_front() {
            return Ok(unsafe {
                drop(inner);
                drop(waiter.as_ref().slot.replace(Slot::Value(value)));
                waiter.as_ref().stored.store(true, Ordering::Release);
            });
        }

        if inner.receivers == 0 {
            return Err(Err(value));
        }

        if inner.queue.len() < inner.capacity {
            inner.queue.push_back(value);
            return Ok(());
        }

        if !block {
            return Err(Ok(value));
        }

        let waiter = Waiter {
            stored: AtomicBool::new(false),
            slot: Cell::new(Slot::Value(value)),
        };

        inner.send_queue.push_back(NonNull::from(&waiter));
        drop(inner);

        wait_until(&waiter.stored, true, Ordering::Acquire);
        match waiter.slot.replace(Slot::Empty) {
            Slot::Empty => Ok(()),
            Slot::Value(_) => unreachable!(),
            Slot::Disconnected(None) => unreachable!(),
            Slot::Disconnected(Some(value)) => Err(Err(value)),
        }
    }

    pub fn recv<T>(chan: &Channel<T>, block: bool) -> Result<T, Result<(), ()>> {
        let mut inner = chan.lock();

        if let Some(waiter) = inner.send_queue.pop_front() {
            return Ok(unsafe {
                drop(inner);
                let value = match waiter.as_ref().slot.replace(Slot::Empty) {
                    Slot::Empty => unreachable!(),
                    Slot::Value(value) => value,
                    Slot::Disconnected(_) => unreachable!(),
                };

                waiter.as_ref().stored.store(true, Ordering::Release);
                value
            });
        }

        if let Some(value) = inner.queue.pop_front() {
            return Ok(value);
        }

        if inner.senders == 0 {
            return Err(Err(()));
        }

        if !block {
            return Err(Ok(()));
        }

        let waiter = Waiter {
            stored: AtomicBool::new(false),
            slot: Cell::new(Slot::Empty),
        };

        inner.recv_queue.push_back(NonNull::from(&waiter));
        drop(inner);

        wait_until(&waiter.stored, true, Ordering::Acquire);
        match waiter.slot.replace(Slot::Empty) {
            Slot::Empty => unreachable!(),
            Slot::Value(value) => Ok(value),
            Slot::Disconnected(None) => Err(Err(())),
            Slot::Disconnected(Some(_)) => unreachable!(),
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct SendError<T>(pub T);

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum TrySendError<T> {
    Full(T),
    Disconnected(T),
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct RecvError;

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum TryRecvError {
    Empty,
    Disconnected,
}

mod unbounded {
    use super::*;

    pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
        let chan = internal::channel(None);
        let sender = Sender { chan: chan.clone() };
        let receiver = Receiver { chan };
        (sender, receiver)
    }
    
    pub struct Sender<T> {
        pub(super) chan: internal::Channel<T>,
    }
    
    impl<T> Sender<T> {
        pub fn send(&self, value: T) -> Result<(), SendError<T>> {
            match internal::send(&self.chan, value, true) {
                Ok(()) => Ok(()),
                Err(Ok(_)) => unreachable!(),
                Err(Err(value)) => Err(SendError(value)),
            }
        }
    }

    impl<T> Clone for Sender<T> {
        fn clone(&self) -> Self {
            internal::clone(&self.chan, true);
            Self { chan: self.chan.clone() }
        }
    }
    
    impl<T> Drop for Sender<T> {
        fn drop(&mut self) {
            internal::disconnect(&self.chan, true);
        }
    }
    
    pub struct Receiver<T> {
        pub(super) chan: internal::Channel<T>,
    }
  
    impl<T> Receiver<T> {
        pub fn try_recv(&self) -> Result<T, TryRecvError> {
            match internal::recv(&self.chan, false) {
                Ok(value) => Ok(value),
                Err(Ok(())) => Err(TryRecvError::Empty),
                Err(Err(())) => Err(TryRecvError::Disconnected),
            }
        }

        pub fn recv(&self) -> Result<T, RecvError> {
            match internal::recv(&self.chan, true) {
                Ok(value) => Ok(value),
                Err(Ok(())) => unreachable!(),
                Err(Err(())) => Err(RecvError),
            }
        }
    }
    
    impl<T> Clone for Receiver<T> {
        fn clone(&self) -> Self {
            internal::clone(&self.chan, false);
            Self { chan: self.chan.clone() }
        }
    }
    
    impl<T> Drop for Receiver<T> {
        fn drop(&mut self) {
            internal::disconnect(&self.chan, false);
        }
    }
}

mod bounded {
    use super::*;
    use unbounded::{Sender, Receiver};

    pub fn channel<T>(capacity: usize) -> (SyncSender<T>, Receiver<T>) {
        let chan = internal::channel(Some(capacity));
        let sender = SyncSender { inner: Sender { chan: chan.clone() } };
        let receiver = Receiver { chan };
        (sender, receiver)
    }
    
    #[derive(Clone)]
    pub struct SyncSender<T> {
        inner: Sender<T>,
    }
    
    impl<T> SyncSender<T> {
        pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
            match internal::send(&self.inner.chan, value, false) {
                Ok(()) => Ok(()),
                Err(Ok(value)) => Err(TrySendError::Full(value)),
                Err(Err(value)) => Err(TrySendError::Disconnected(value)),
            }
        }

        pub fn send(&self, value: T) -> Result<(), SendError<T>> {
            self.inner.send(value)
        }
    }
}

pub use unbounded::{Sender, Receiver, channel};
pub use bounded::{SyncSender, channel as sync_channel};