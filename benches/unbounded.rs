use std::{
    sync::atomic::{AtomicBool, fence, Ordering},
    thread,
};

use criterion::{criterion_group, criterion_main, Criterion};

struct Chan {
    thread: thread::Thread,
    unparked: AtomicBool,
}

impl Chan {
    fn new() -> Self {
        Self {
            thread: thread::current(),
            unparked: AtomicBool::new(false),
        }
    }

    fn send<V, T>(&self, x: &T, f: impl Fn(&T) -> Result<(), V>) -> Result<(), V> {
        f(&x).map(|_| {
            fence(Ordering::SeqCst);

            self.unparked
                .fetch_update(Ordering::Release, Ordering::Relaxed, |p| (!p).then(|| true))
                .map(|_| self.thread.unpark())
                .unwrap_or(())
        })
    }

    fn recv<V, T>(&self, x: &T, mut f: impl FnMut(&T) -> Option<V>) -> Option<V> {
        loop {
            if let Some(v) = f(x) {
                return Some(v);
            }

            fence(Ordering::SeqCst);

            while !self.unparked.swap(false, Ordering::Acquire) {
                thread::park();
            }
        }
    }
}

fn mpsc_bounded(c: &mut Criterion) {
    let mut group = c.benchmark_group("mpsc");
    group.sample_size(20);

    const THREADS: usize = 14;
    const MESSAGES: usize = 14 * 200_000;

    group.bench_function("jiffy", |b| {
        b.iter(|| {
            let q = jiffy::unbounded::Queue::new();
            let c = Chan::new();

            crossbeam::scope(|scope| {
                for _ in 0..THREADS {
                    scope.spawn({
                        |_| {
                            for i in 0..MESSAGES / THREADS {
                                c.send(&q, |c| Ok::<_, ()>(c.push(i))).unwrap();
                            }
                        }
                    });
                }

                for _ in 0..MESSAGES {
                    c.recv(&q, |c| c.pop()).unwrap();
                }
            })
            .unwrap();
        })
    });

    group.bench_function("crossbeam", |b| {
        b.iter(|| {
            let t = crossbeam::queue::SegQueue::new();
            let c = Chan::new();

            crossbeam::scope(|scope| {
                for _ in 0..THREADS {
                    scope.spawn({
                        |_| {
                            for i in 0..MESSAGES / THREADS {
                                c.send(&t, |c| Ok::<_, ()>(c.push(i))).unwrap();
                            }
                        }
                    });
                }

                for _ in 0..MESSAGES {
                    c.recv(&t, |c| c.pop()).unwrap();
                }
            })
            .unwrap();
        })
    });

    group.bench_function("block-mpsc", |b| {
        b.iter(|| {
            let t = jiffy::block::Queue::EMPTY;
            let c = Chan::new();

            crossbeam::scope(|scope| {
                for _ in 0..THREADS {
                    scope.spawn({
                        |_| {
                            for i in 0..MESSAGES / THREADS {
                                c.send(&t, |c| Ok::<_, ()>(c.send(i))).unwrap();
                            }
                        }
                    });
                }

                for _ in 0..MESSAGES {
                    unsafe {
                        c.recv(&t, |c| c.try_recv()).unwrap();
                    }
                }
            })
            .unwrap();
        })
    });

    group.bench_function("protty-looish", |b| {
        b.iter(|| {
            let t = jiffy::protty::Queue::EMPTY;
            let c = Chan::new();

            crossbeam::scope(|scope| {
                for _ in 0..THREADS {
                    scope.spawn({
                        |_| {
                            for i in 0..MESSAGES / THREADS {
                                c.send(&t, |c| Ok::<_, ()>(c.send(i))).unwrap();
                            }
                        }
                    });
                }

                for _ in 0..MESSAGES {
                    unsafe {
                        c.recv(&t, |c| c.try_recv()).unwrap();
                    }
                }
            })
            .unwrap();
        })
    });

    group.finish();
}

criterion_group!(benches, mpsc_bounded);
criterion_main!(benches);
