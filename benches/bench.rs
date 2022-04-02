use std::{
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Barrier},
    thread,
};

use criterion::{criterion_group, criterion_main, Criterion};

const THREADS: usize = 15;
const MESSAGES: usize = 15 * 100_000;

struct Chan<T> {
    thread: thread::Thread,
    unparked: AtomicBool,
    inner: T,
}

impl<T> Chan<T> {
    fn new(inner: T) -> Self {
        Self {
            thread: thread::current(),
            unparked: AtomicBool::new(false),
            inner,
        }
    }

    fn send<V, F: Fn(&T) -> Result<(), V>>(&self, f: F) -> Result<(), V> {
        f(&self.inner).map(|_| self.unpark())
    }

    fn recv<V>(&self, f: impl Fn(&T) -> Option<V>) -> Option<V> {
        loop {
            match f(&self.inner) {
                Some(x) => return Some(x),
                None => self.park(),
            }
        }
    }

    fn park(&self) {
        while !self.unparked.swap(false, Ordering::Acquire) {
            thread::park();
        }
    }

    fn unpark(&self) {
        if !self.unparked.swap(true, Ordering::Release) {
            self.thread.unpark();
        }
    }
}

fn mpsc(c: &mut Criterion) {
    let mut group = c.benchmark_group("mpsc");
    group.sample_size(10);

    group.bench_function("protty-mpsc", |b| {
        b.iter(|| {
            let queue = Chan::new(jiffy::protty_mpsc::MpscQueue::default());
            let barrier = Barrier::new(THREADS + 1);

            crossbeam::scope(|scope| {
                for _ in 0..THREADS {
                    scope.spawn(|_| {
                        barrier.wait();
                        for i in 0..MESSAGES / THREADS {
                            queue.send::<usize, _>(|c| Ok(c.push(i))).unwrap();
                        }
                    });
                }

                barrier.wait();
                for _ in 0..MESSAGES {
                    queue.recv(|c| unsafe { c.pop() }).unwrap();
                }
            })
            .unwrap();
        })
    });

    group.bench_function("crossbeam-queue", |b| {
        b.iter(|| {
            let queue = Chan::new(crossbeam::queue::SegQueue::new());
            let barrier = Barrier::new(THREADS + 1);

            crossbeam::scope(|scope| {
                for _ in 0..THREADS {
                    scope.spawn(|_| {
                        barrier.wait();
                        for i in 0..MESSAGES / THREADS {
                            queue.send(|c| Ok::<_, ()>(c.push(i))).unwrap();
                        }
                    });
                }

                barrier.wait();
                for _ in 0..MESSAGES {
                    queue.recv(|c| c.pop()).unwrap();
                }
            })
            .unwrap();
        })
    });

    group.bench_function("std", |b| {
        b.iter(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            let barrier = Arc::new(Barrier::new(THREADS + 1));

            crossbeam::scope(|scope| {
                for _ in 0..THREADS {
                    let tx = tx.clone();
                    let barrier = barrier.clone();

                    scope.spawn(move |_| {
                        barrier.wait();
                        for i in 0..MESSAGES / THREADS {
                            tx.send(i).unwrap();
                        }
                    });
                }

                barrier.wait();
                for _ in 0..MESSAGES {
                    rx.recv().unwrap();
                }
            })
            .unwrap();
        })
    });

    group.bench_function("flume", |b| {
        b.iter(|| {
            let (tx, rx) = flume::unbounded();
            let barrier = Barrier::new(THREADS + 1);

            crossbeam::scope(|scope| {
                for _ in 0..THREADS {
                    scope.spawn(|_| {
                        barrier.wait();
                        for i in 0..MESSAGES / THREADS {
                            tx.send(i).unwrap();
                        }
                    });
                }

                barrier.wait();
                for _ in 0..MESSAGES {
                    rx.recv().unwrap();
                }
            })
            .unwrap();
        })
    });

    group.finish();
}

criterion_group!(benches, mpsc);
criterion_main!(benches);
