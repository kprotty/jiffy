use std::{
    sync::atomic::{AtomicBool, Ordering},
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
        f(&x).map(|_| self.unpark())
    }

    fn recv<V, T>(&self, x: &T, f: impl Fn(&T) -> Option<V>) -> Option<V> {
        loop {
            match f(x) {
                Some(x) => break Some(x),
                None => {
                    while !self.try_unpark() {
                        thread::park();
                    }
                }
            }
        }
    }

    fn try_unpark(&self) -> bool {
        self.unparked.swap(false, Ordering::Acquire)
    }

    fn unpark(&self) {
        if !self.unparked.swap(true, Ordering::Release) {
            self.thread.unpark();
        }
    }
}

fn mpsc_bounded(c: &mut Criterion) {
    let mut group = c.benchmark_group("mpsc");
    group.sample_size(20);

    const THREADS: usize = 4;
    const MESSAGES: usize = THREADS * 5_000_000;

    fn record_latency(mut f: impl FnMut(usize)) {
        // let mut elapsed = Vec::with_capacity(MESSAGES / THREADS + 1);
        // for i in 0..MESSAGES / THREADS + 1 {
        //     let start = std::time::Instant::now();
        //     f(i);
        //     elapsed.push(start.elapsed());
        // }

        // let sum: std::time::Duration = elapsed.iter().sum();
        // let avg_ns = sum.as_nanos() / (elapsed.len() as u128);
        
        // print!("\nsend(): min={:?} avg={:?} max={:?}",
        //     elapsed.iter().min().unwrap(),
        //     std::time::Duration::from_nanos(avg_ns as u64),
        //     elapsed.iter().max().unwrap(),
        // )

        for i in 0..MESSAGES / THREADS + 1 {
            f(i);
        }
    }

    // group.bench_function("lock_mpmc", |b| {
    //     b.iter(|| {
    //         let (tx, rx) = jiffy::lock_mpmc::channel();

    //         crossbeam::scope(|scope| {
    //             for _ in 0..THREADS {
    //                 let tx = tx.clone();
    //                 scope.spawn({
    //                     move |_| {
    //                         record_latency(|i| {
    //                             tx.send(i).unwrap();
    //                         });
    //                     }
    //                 });
    //             }

    //             for _ in 0..MESSAGES {
    //                 rx.recv().unwrap();
    //             }
    //         })
    //         .unwrap();
    //     })
    // });

    group.bench_function("ploo_mpsc", |b| {
        b.iter(|| {
            let t = jiffy::ploo_mpsc::Queue::EMPTY;
            let c = Chan::new();

            crossbeam::scope(|scope| {
                for _ in 0..THREADS {
                    scope.spawn({
                        |_| {
                            record_latency(|i| {
                                c.send(&t, |c| Ok::<_, ()>(c.send(i))).unwrap();
                            });
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

    group.bench_function("kanal", |b| {
        b.iter(|| {
            let (tx, rx) = kanal::unbounded();

            crossbeam::scope(|scope| {
                for _ in 0..THREADS {
                    let tx = tx.clone();
                    scope.spawn({
                        move |_| {
                            record_latency(|i| {
                                tx.send(i).unwrap();
                            });
                        }
                    });
                }

                for _ in 0..MESSAGES {
                    rx.recv().unwrap();
                }
            })
            .unwrap();
        })
    });

    group.bench_function("block_mpsc", |b| {
        b.iter(|| {
            let t = jiffy::block_mpsc::Queue::EMPTY;
            let c = Chan::new();

            crossbeam::scope(|scope| {
                for _ in 0..THREADS {
                    scope.spawn({
                        |_| {
                            record_latency(|i| {
                                c.send(&t, |c| Ok::<_, ()>(c.send(i))).unwrap();
                            });
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

    // group.bench_function("flume", |b| {
    //     b.iter(|| {
    //         let (tx, rx) = flume::unbounded();

    //         crossbeam::scope(|scope| {
    //             for _ in 0..THREADS {
    //                 let tx = tx.clone();
    //                 scope.spawn({
    //                     move |_| {
    //                         record_latency(|i| {
    //                             tx.send(i).unwrap();
    //                         });
    //                     }
    //                 });
    //             }

    //             for _ in 0..MESSAGES {
    //                 rx.recv().unwrap();
    //             }
    //         })
    //         .unwrap();
    //     })
    // });

    // group.bench_function("crossbeam", |b| {
    //     b.iter(|| {
    //         let t = crossbeam::queue::SegQueue::new();
    //         let c = Chan::new();

    //         crossbeam::scope(|scope| {
    //             for _ in 0..THREADS {
    //                 scope.spawn({
    //                     |_| {
    //                         record_latency(|i| {
    //                             c.send(&t, |c| Ok::<_, ()>(c.push(i))).unwrap();
    //                         });
    //                     }
    //                 });
    //             }

    //             for _ in 0..MESSAGES {
    //                 c.recv(&t, |c| c.pop()).unwrap();
    //             }
    //         })
    //         .unwrap();
    //     })
    // });

    group.bench_function("jiffy", |b| {
        b.iter(|| {
            let q = jiffy::unbounded::Queue::new();
            let c = Chan::new();
            let b = std::sync::Barrier::new(THREADS);

            crossbeam::scope(|scope| {
                for _ in 0..THREADS {
                    scope.spawn({
                        |_| {
                            b.wait();
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

    // group.bench_function("riffy (unsound)", |b| {
    //     b.iter(|| {
    //         let t = riffy::MpscQueue::new();
    //         let c = Chan::new();

    //         crossbeam::scope(|scope| {
    //             for _ in 0..THREADS {
    //                 scope.spawn({
    //                     |_| {
    //                         for i in 0..MESSAGES / THREADS {
    //                             c.send(&t, |c| c.enqueue(i)).unwrap();
    //                         }
    //                     }
    //                 });
    //             }

    //             for _ in 0..MESSAGES {
    //                 c.recv(&t, |c| c.dequeue()).unwrap();
    //             }
    //         })
    //         .unwrap();
    //     })
    // });


    group.finish();
}

criterion_group!(benches, mpsc_bounded);
criterion_main!(benches);
