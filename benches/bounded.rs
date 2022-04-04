use std::{
    sync::atomic::{AtomicBool, Ordering},
    sync::Barrier,
    thread,
};

use criterion::{criterion_group, criterion_main, Criterion};

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

    fn send<V>(&self, f: impl Fn(&T) -> Result<(), V>) -> Result<(), V> {
        f(&self.inner).map(|_| self.unpark())
    }

    fn recv<V>(&self, f: impl Fn(&T) -> Option<V>) -> Option<V> {
        let mut counter = 0;
        loop {
            match f(&self.inner) {
                Some(x) => break Some(x),
                None => {
                    if counter <= 3 {
                        counter += 1;
                        (0..(1<<counter)).for_each(|_| std::hint::spin_loop());
                    } else {
                        std::thread::yield_now();
                    }
                    
                    // while !self.try_unpark() {
                    //     thread::park();
                    // }
                }
            }
        }
    }

    // fn try_unpark(&self) -> bool {
    //     self.unparked.swap(false, Ordering::Acquire)
    // }

    fn unpark(&self) {
        // self.unparked
        //     .fetch_update(Ordering::Release, Ordering::Relaxed, |unparked| {
        //         if unparked {
        //             None
        //         } else {
        //             Some(true)
        //         }
        //     })
        //     .map(|_| self.thread.unpark())
        //     .unwrap_or(());
    }
}

fn mpsc(c: &mut Criterion) {
    let threads = num_cpus::get() - 2;
    let messages = threads * 50_000;

    let mut group = c.benchmark_group("mpsc");

    group.bench_function("vyukov", |b| {
        b.iter(|| {
            let queue = Chan::new(jiffy::vyukov::Queue::new(messages));
            let barrier = Barrier::new(threads + 1);

            crossbeam::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|_| {
                        barrier.wait();
                        for i in 0..messages / threads {
                            queue.send(|c| c.push(i)).unwrap();
                        }
                    });
                }

                barrier.wait();
                for i in 0..messages {
                    match queue.recv(|c| unsafe { c.pop() }) {
                        Some(_) => {}
                        None => panic!("deadlocked at {}/{}", i, messages),
                    }
                }
            })
            .unwrap();
        })
    });

    group.bench_function("basic", |b| {
        b.iter(|| {
            let queue = Chan::new(jiffy::basic::Queue::new(messages));
            let barrier = Barrier::new(threads + 1);

            crossbeam::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|_| {
                        barrier.wait();
                        for i in 0..messages / threads {
                            queue.send(|c| c.push(i)).unwrap();
                        }
                    });
                }

                barrier.wait();
                for i in 0..messages {
                    match queue.recv(|c| unsafe { c.pop() }) {
                        Some(_) => {}
                        None => panic!("deadlocked at {}/{}", i, messages),
                    }
                }
            })
            .unwrap();
        })
    });

    group.bench_function("jiffy", |b| {
        b.iter(|| {
            let queue = Chan::new(jiffy::bounded::Queue::new(messages));
            let barrier = Barrier::new(threads + 1);

            crossbeam::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|_| {
                        barrier.wait();
                        for i in 0..messages / threads {
                            queue.send(|c| c.push(i)).unwrap();
                        }
                    });
                }

                barrier.wait();
                for _ in 0..messages {
                    queue.recv(|c| c.pop()).unwrap();
                }
            })
            .unwrap();
        })
    });

    group.bench_function("crossbeam", |b| {
        b.iter(|| {
            let (tx, rx) = crossbeam::channel::bounded(messages);
            let barrier = Barrier::new(threads + 1);

            crossbeam::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|_| {
                        barrier.wait();
                        for i in 0..messages / threads {
                            tx.try_send(i).unwrap();
                        }
                    });
                }

                barrier.wait();
                for _ in 0..messages {
                    rx.recv().unwrap();
                }
            })
            .unwrap();
        })
    });

    group.bench_function("std", |b| {
        b.iter(|| {
            let (tx, rx) = std::sync::mpsc::sync_channel(messages);
            let barrier = Barrier::new(threads + 1);

            crossbeam::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|_| {
                        barrier.wait();
                        for i in 0..messages / threads {
                            tx.try_send(i).unwrap();
                        }
                    });
                }

                barrier.wait();
                for _ in 0..messages {
                    rx.recv().unwrap();
                }
            })
            .unwrap();
        })
    });

    group.bench_function("flume", |b| {
        b.iter(|| {
            let (tx, rx) = flume::bounded(messages);
            let barrier = Barrier::new(threads + 1);

            crossbeam::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|_| {
                        barrier.wait();
                        for i in 0..messages / threads {
                            tx.try_send(i).unwrap();
                        }
                    });
                }

                barrier.wait();
                for _ in 0..messages {
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
