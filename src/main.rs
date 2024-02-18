use lock_free_queue::*;
use std::{sync::atomic::*, sync::Arc, thread};

fn main() {
    let (producer, consumer) = Queue::<usize, 256>::new();
    let polling = Arc::new(AtomicBool::new(true));
    let _ = ctrlc::set_handler({
        let polling = polling.clone();
        move || {
            polling.store(false, Ordering::Relaxed);
        }
    });

    thread::scope(|s| {
        s.spawn({
            let producer = producer.clone();
            move || {
                for i in 0..1000 {
                    let timeout = std::time::Duration::new(5, 0);
                    match producer.send(i, Some(timeout)) {
                        Err(e) => println!("{:?}", e),
                        _ => println!("INSERT: {}", i),
                    }
                }
            }
        });
        s.spawn({
            let producer = producer.clone();
            move || {
                for i in 1000..2000 {
                    let timeout = std::time::Duration::new(5, 0);
                    match producer.send(i, Some(timeout)) {
                        Err(e) => println!("{:?}", e),
                        _ => println!("INSERT: {}", i),
                    }
                }
            }
        });
        s.spawn({
            let consumer = consumer.clone();
            let polling = polling.clone();
            move || {
                while polling.load(Ordering::Relaxed) {
                    match consumer.receive(None) {
                        Ok(v) => println!("POP {}", v),
                        Err(e) => {
                            println!("{:?}", e);
                            return;
                        }
                    }
                }
            }
        });
    });
}
