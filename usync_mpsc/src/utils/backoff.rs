use std::{
    hint::spin_loop,
    num::NonZeroUsize,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
};

#[derive(Default)]
pub struct Backoff {
    attempt: usize,
}

impl Backoff {
    pub const fn new() -> Self {
        Self { attempt: 0 }
    }

    pub fn try_yield_now(&mut self) -> bool {
        // Don't spin if the platform says there's no useful concurrency.
        if Self::num_cpus().get() == 1 {
            return false;
        }

        // Decide the maximum spin attempts.
        let mut max_cpu_spin = 0;
        if cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
            max_cpu_spin = 32;
        }

        if self.attempt >= max_cpu_spin {
            return false;
        }

        self.attempt += 1;
        spin_loop();
        true
    }

    pub fn yield_now(&mut self) {
        // Don't spin if the platform says there's no useful concurrency.
        if Self::num_cpus().get() == 1 {
            return;
        }

        // Decide the maximum spin attempts.
        let mut max_cpu_spin = 3;
        if cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
            max_cpu_spin = 6;
        }

        // On windows, SwitchToThread() seems to be always worse than CPU spinning.
        // On linux, sched_yield() is generally unspecified behavior so avoid it there too.
        let always_spin = cfg!(any(
            target_os = "windows",
            target_os = "linux",
            target_os = "android",
        ));

        if self.attempt <= max_cpu_spin || always_spin {
            // Bound to at most 1<<10 = 1024 for windows spinning.
            for _ in 0..(1 << self.attempt.min(10)) {
                spin_loop();
            }

            self.attempt = self.attempt.wrapping_add(1);
            return;
        }

        // Start (trying) to yield the thread's quota.
        thread::yield_now();
    }

    fn num_cpus() -> NonZeroUsize {
        static CPUS: AtomicUsize = AtomicUsize::new(0);

        NonZeroUsize::new(CPUS.load(Ordering::Relaxed)).unwrap_or_else(|| {
            let cpus = thread::available_parallelism()
                .ok()
                .or(NonZeroUsize::new(1))
                .unwrap();

            CPUS.store(cpus.get(), Ordering::Relaxed);
            cpus
        })
    }
}
