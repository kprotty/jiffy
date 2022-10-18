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
                        time: std::os::raw::c_int,
                    ) -> std::os::raw::c_int;
                }
                const SWITCH_OPTION_DEPRESS: std::os::raw::c_int = 1;
                unsafe { thread_switch(std::ptr::null_mut(), SWITCH_OPTION_DEPRESS, spins) };
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
        for i in 0.. {
            match () {
                _ if i <= 3 => (0..(1 << i)).for_each(|_| std::hint::spin_loop()),
                _ => std::thread::yield_now(),
            }

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

    type Guard = lock_api::GuardSend;

    #[cfg(all(feature = "os_lock", target_os = "windows"))]
    mod lock {
        use std::{cell::UnsafeCell, ptr::null_mut, os::raw::c_void};

        extern "system" {
            fn AcquireSRWLockExclusive(p: *mut *mut c_void);
            fn ReleaseSRWLockExclusive(p: *mut *mut c_void);
        }

        pub struct RawMutex {
            srwlock: UnsafeCell<*mut c_void>,
        }

        unsafe impl Send for RawMutex {}
        unsafe impl Sync for RawMutex {}

        unsafe impl lock_api::RawMutex for RawMutex {
            const INIT: Self = Self {
                srwlock: UnsafeCell::new(null_mut()),
            };
    
            type GuardMarker = super::Guard;
            
            fn try_lock(&self) -> bool {
                unreachable!()
            }
    
            fn lock(&self) {
                unsafe { AcquireSRWLockExclusive(self.srwlock.get()) }
            }
    
            unsafe fn unlock(&self) {
                ReleaseSRWLockExclusive(self.srwlock.get())
            }
        }
    }

    #[cfg(all(feature = "os_lock", target_vendor = "apple"))]
    mod lock {
        use std::{cell::UnsafeCell, ptr::null_mut};

        extern "C" {
            fn os_unfair_lock_lock(p: *mut u32);
            fn os_unfair_lock_unlock(p: *mut u32);
        }

        pub struct RawMutex {
            oul: UnsafeCell<u32>,
        }

        unsafe impl Send for RawMutex {}
        unsafe impl Sync for RawMutex {}

        unsafe impl lock_api::RawMutex for RawMutex {
            const INIT: Self = Self {
                oul: UnsafeCell::new(0),
            };
    
            type GuardMarker = super::Guard;
            
            fn try_lock(&self) -> bool {
                unreachable!()
            }
    
            fn lock(&self) {
                unsafe { os_unfair_lock_lock(self.oul.get()) }
            }
    
            unsafe fn unlock(&self) {
                os_unfair_lock_unlock(self.oul.get())
            }
        }
    }

    #[cfg(all(feature = "os_lock", any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "emscripten",
        target_os = "fuchsia",
    )))]
    mod lock {
        use std::{sync::atomic::{AtomicU32, Ordering}};

        const UNLOCKED: u32 = 0;
        const LOCKED: u32 = 1;
        const CONTENDED: u32 = 2;

        pub struct RawMutex {
            state: AtomicU32,
        }

        unsafe impl lock_api::RawMutex for RawMutex {
            const INIT: Self = Self {
                state: AtomicU32::new(UNLOCKED),
            };
    
            type GuardMarker = super::Guard;
            
            fn try_lock(&self) -> bool {
                unreachable!()
            }
    
            fn lock(&self) {
                if let Err(state) = self.state.compare_exchange_weak(
                    UNLOCKED,
                    LOCKED,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    if state == CONTENDED {
                        futex::wait(&self.state, CONTENDED);
                    }

                    while self.state.swap(CONTENDED, Ordering::Acquire) != UNLOCKED {
                        futex::wait(&self.state, CONTENDED);
                    }
                }
            }
    
            unsafe fn unlock(&self) {
                if self.state.swap(UNLOCKED, Ordering::Release) == CONTENDED {
                    futex::wake(&self.state, 1);
                }
            }
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
        mod futex {
            fn wait(ptr: &super::AtomicU32, cmp: u32) {
                let _ = unsafe {
                    libc::syscall(
                        libc::SYS_futex,
                        ptr as *const _ as *mut _,
                        libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                        cmp,
                        std::ptr::null::<libc::timespec>(),
                    )
                };
            }

            fn wake(ptr: &super::AtomicU32, n: u32) {
                let _ = unsafe {
                    libc::syscall(
                        libc::SYS_futex,
                        ptr as *const _ as *mut _,
                        libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
                        n,
                    )
                };
            }
        }

        #[cfg(target_os = "freebsd")]
        mod futex {
            fn wait(ptr: &super::AtomicU32, cmp: u32) {
                let _ = unsafe {
                    libc::_umtx_op(
                        ptr as *const _ as *mut _,
                        libc::UMTX_OP_WAIT_UINT_PRIVATE,
                        cmp as libc::c_ulong,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
            }

            fn wake(ptr: &super::AtomicU32, n: u32) {
                let _ = unsafe {
                    libc::_umtx_op(
                        ptr as *const _ as *mut _,
                        libc::UMTX_OP_WAKE_PRIVATE,
                        n as libc::c_ulong,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
            }
        }

        #[cfg(target_os = "openbsd")]
        mod futex {
            fn wait(ptr: &super::AtomicU32, cmp: u32) {
                let _ = unsafe {
                    libc::futex(
                        ptr as *const _ as *mut u32,
                        libc::FUTEX_WAIT,
                        cmp as i32,
                        std::ptr::null(),
                        std::ptr::null_mut(),
                    )
                };
            }

            fn wake(ptr: &super::AtomicU32, n: u32) {
                let _ = unsafe {
                    libc::futex(
                        ptr as *const _ as *mut u32,
                        libc::FUTEX_WAKE,
                        n as i32,
                        std::ptr::null(),
                        std::ptr::null_mut(),
                    )
                };
            }
        }

        #[cfg(target_os = "dragonfly")]
        mod futex {
            fn wait(ptr: &super::AtomicU32, cmp: u32) {
                let _ = unsafe {
                    libc::umtx_sleep(
                        ptr as *const _ as *const i32,
                        cmp as i32,
                        0i32,
                    )
                };
            }

            fn wake(ptr: &super::AtomicU32, n: u32) {
                let _ = unsafe {
                    libc::umtx_wakeup(
                        ptr as *const _ as *const i32,
                        n as i32,
                    )
                };
            }
        }

        #[cfg(target_os = "emscripten")]
        mod futex {
            extern "C" {
                fn emscripten_futex_wake(addr: *const super::AtomicU32, count: libc::c_int) -> libc::c_int;
                fn emscripten_futex_wait(
                    addr: *const super::AtomicU32,
                    val: libc::c_uint,
                    max_wait_ms: libc::c_double,
                ) -> libc::c_int;
            }

            fn wait(ptr: &super::AtomicU32, cmp: u32) {
                let _ = unsafe {
                    emscripten_futex_wait(
                        ptr,
                        cmp as libc::c_uint,
                        f64::INFINITY,
                    ) != -libc::ETIMEDOUT
                };
            }

            fn wake(ptr: &super::AtomicU32, n: u32) {
                let _ = unsafe {
                    emscripten_futex_wait(
                        ptr,
                        n as libc::c_int,
                    )
                };
            }
        }

        #[cfg(target_os = "fuchsia")]
        mod futex {
            type zx_futex_t = super::AtomicU32;
            type zx_handle_t = u32;
            type zx_status_t = i32;
            type zx_time_t = i64;

            const ZX_HANDLE_INVALID: zx_handle_t = 0;
            const ZX_TIME_INFINITE: zx_time_t = zx_time_t::MAX;

            extern "C" {
                fn zx_futex_wait(
                    value_ptr: *const zx_futex_t,
                    current_value: zx_futex_t,
                    new_futex_owner: zx_handle_t,
                    deadline: zx_time_t,
                ) -> zx_status_t;
                fn zx_futex_wake(value_ptr: *const zx_futex_t, wake_count: u32) -> zx_status_t;
            }

            fn wait(ptr: &super::AtomicU32, cmp: u32) {
                let _ = unsafe {
                    zx_futex_wait(
                        ptr,
                        super::AtomicU32::new(cmp),
                        ZX_HANDLE_INVALID,
                        ZX_TIME_INFINITE,
                    )
                };
            }

            fn wake(ptr: &super::AtomicU32, n: u32) {
                let _ = unsafe {
                    zx_futex_wake(
                        ptr,
                        n,
                    )
                };
            }
        }
    }

    #[cfg(any(not(feature = "os_lock"), not(any(
        target_os = "windows",
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "emscripten",
        target_os = "fuchsia",
    ))))]
    mod lock {
        use super::{AtomicBool, Ordering, wait_until};

        pub struct RawMutex {
            locked: AtomicBool,
        }

        unsafe impl lock_api::RawMutex for RawMutex {
            const INIT: Self = Self {
                locked: AtomicBool::new(false),
            };
    
            type GuardMarker = super::Guard;
            
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
    
    pub type Mutex<T> = lock_api::Mutex<lock::RawMutex, T>;
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