use std::{
    array::from_fn,
    cell::UnsafeCell,
    mem::replace,
    sync::{
        atomic::{
            AtomicIsize, AtomicU8, AtomicUsize,
            Ordering::{self, Acquire, Relaxed, Release},
        },
        Arc,
    },
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct Queue<T, const SIZE: usize> {
    buf: UnsafeArr<T, SIZE>,
    enqueue_sems: UnsafeArr<Semaphore, SIZE>,
    dequeue_sems: UnsafeArr<Semaphore, SIZE>,
    lower_bound_counter: AtomicIsize,
    upper_bound_counter: AtomicIsize,
    head: AtomicUsize,
    tail: AtomicUsize,
    sender_count: AtomicUsize,
    sender_op_failure_count: AtomicUsize,
    receiver_count: AtomicUsize,
    receiver_op_failure_count: AtomicUsize,
    state: AtomicU8,
}

impl<T, const SIZE: usize> Queue<T, SIZE>
where
    T: Default,
{
    const MASK: usize = SIZE - 1;
    pub fn new() -> (Sender<T, SIZE>, Receiver<T, SIZE>) {
        assert!(SIZE & (SIZE - 1) == 0, "SIZE: {} is not a power of 2", SIZE);
        assert!(SIZE < std::isize::MAX as usize, "SIZE is too large");
        let queue = Arc::new(Queue::<T, SIZE> {
            buf: UnsafeArr::new(|_| T::default()),
            enqueue_sems: UnsafeArr::new(|_| Semaphore::new(1, 1)),
            dequeue_sems: UnsafeArr::new(|_| Semaphore::new(0, 1)),
            lower_bound_counter: AtomicIsize::new(0),
            upper_bound_counter: AtomicIsize::new(0),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            sender_count: AtomicUsize::new(0),
            sender_op_failure_count: AtomicUsize::new(0),
            receiver_count: AtomicUsize::new(0),
            receiver_op_failure_count: AtomicUsize::new(0),
            state: AtomicU8::new(QueueState::Healthy as u8),
        });

        (
            Sender {
                queue: queue.clone(),
            },
            Receiver {
                queue: queue.clone(),
            },
        )
    }

    fn enqueue(&self, elem: T) -> Result<(), QueueErr> {
        if test_increment_retest(&self.upper_bound_counter, 1, SIZE, Acquire) {
            let index = (self.tail.fetch_add(1, Relaxed)) & Self::MASK;
            self.enqueue_sems[index].acquire_permits(1).unwrap();
            self.buf.inner()[index] = elem;
            self.dequeue_sems[index].add_permits(1).unwrap();
            self.lower_bound_counter.fetch_add(1, Release);

            Ok(())
        } else {
            Err(QueueErr::QueueOverflow)
        }
    }

    fn dequeue(&self) -> Result<T, QueueErr> {
        if test_decrement_retest(&self.lower_bound_counter, 1, Acquire) {
            let index = (self.head.fetch_add(1, Relaxed)) & Self::MASK;
            self.dequeue_sems[index].acquire_permits(1).unwrap();
            let popped = replace(&mut self.buf.inner()[index], T::default());
            self.enqueue_sems[index].add_permits(1).unwrap();
            self.upper_bound_counter.fetch_sub(1, Release);

            Ok(popped)
        } else {
            Err(QueueErr::QueueUnderflow)
        }
    }

    fn are_senders_blocked(&self) -> bool {
        let blocked = self.sender_op_failure_count.load(Relaxed) == self.sender_count.load(Relaxed);
        if blocked {
            self.state.store(QueueState::SendersBlocked as u8, Relaxed);
        }

        blocked
    }

    fn are_receivers_blocked(&self) -> bool {
        let blocked =
            self.receiver_op_failure_count.load(Relaxed) == self.receiver_count.load(Relaxed);
        if blocked {
            self.state
                .store(QueueState::ReceiversBlocked as u8, Relaxed);
        }

        blocked
    }
}

unsafe impl<T, const SIZE: usize> Send for Queue<T, SIZE> {}
unsafe impl<T, const SIZE: usize> Sync for Queue<T, SIZE> where T: Send {}

pub struct Sender<T, const SIZE: usize> {
    queue: Arc<Queue<T, SIZE>>,
}

impl<T, const SIZE: usize> Sender<T, SIZE>
where
    T: Clone + Default,
{
    pub fn send(&self, elem: T, timeout: Option<Duration>) -> Result<(), SenderErr> {
        let deadline = match timeout {
            Some(t) => Instant::now().checked_add(t),
            None => None,
        };
        let elem_clone = elem.clone();
        if self.queue.enqueue(elem_clone).is_err() {
            self.queue.sender_op_failure_count.fetch_add(1, Relaxed);

            while let Err(_) = {
                let elem_clone = elem.clone();
                self.queue.enqueue(elem_clone)
            } {
                match deadline {
                    Some(d) => {
                        if Instant::now() >= d {
                            if self.queue.are_receivers_blocked() {
                                return Err(SenderErr::QueueFull);
                            }
                            return Err(SenderErr::Timeout);
                        }
                    }
                    None => {
                        if self.queue.are_receivers_blocked() {
                            return Err(SenderErr::QueueFull);
                        }
                    }
                }
            }

            self.queue.sender_op_failure_count.fetch_sub(1, Relaxed);
        }
        Ok(())
    }
}

impl<T, const SIZE: usize> Clone for Sender<T, SIZE> {
    fn clone(&self) -> Self {
        self.queue.sender_count.fetch_add(1, Relaxed);
        Self {
            queue: self.queue.clone(),
        }
    }
}

impl<T, const SIZE: usize> Drop for Sender<T, SIZE> {
    fn drop(&mut self) {
        self.queue.sender_count.fetch_sub(1, Relaxed);
    }
}

#[derive(Debug)]
pub enum SenderErr {
    Timeout,
    QueueFull,
}

pub struct Receiver<T, const SIZE: usize> {
    queue: Arc<Queue<T, SIZE>>,
}

impl<T, const SIZE: usize> Receiver<T, SIZE>
where
    T: Clone + Default,
{
    pub fn receive(&self, timeout: Option<Duration>) -> Result<T, ReceiverErr> {
        let deadline = match timeout {
            Some(t) => Instant::now().checked_add(t),
            None => None,
        };

        match self.queue.dequeue() {
            Ok(elem) => Ok(elem),
            Err(_) => {
                self.queue.receiver_op_failure_count.fetch_add(1, Relaxed);

                loop {
                    match self.queue.dequeue() {
                        Ok(elem) => {
                            self.queue.receiver_op_failure_count.fetch_min(1, Relaxed);
                            return Ok(elem);
                        }
                        Err(_) => match deadline {
                            Some(d) => {
                                if Instant::now() >= d {
                                    if self.queue.are_receivers_blocked() {
                                        return Err(ReceiverErr::QueueEmpty);
                                    }
                                    return Err(ReceiverErr::Timeout);
                                }
                            }
                            None => {
                                if self.queue.are_receivers_blocked() {
                                    return Err(ReceiverErr::QueueEmpty);
                                }
                            }
                        },
                    }
                }
            }
        }
    }
}

impl<T, const SIZE: usize> Clone for Receiver<T, SIZE> {
    fn clone(&self) -> Self {
        self.queue.receiver_count.fetch_add(1, Relaxed);
        Self {
            queue: self.queue.clone(),
        }
    }
}

impl<T, const SIZE: usize> Drop for Receiver<T, SIZE> {
    fn drop(&mut self) {
        self.queue.receiver_count.fetch_sub(1, Relaxed);
    }
}

#[derive(Debug)]
pub enum ReceiverErr {
    Timeout,
    QueueEmpty,
}

#[derive(Debug)]
enum QueueState {
    Healthy,
    SendersBlocked,
    ReceiversBlocked,
}

#[derive(Debug)]
pub enum QueueErr {
    QueueOverflow,
    QueueUnderflow,
}

#[derive(Debug)]
struct UnsafeArr<T, const SIZE: usize> {
    inner: UnsafeCell<Box<[T; SIZE]>>,
}

impl<T, const SIZE: usize> UnsafeArr<T, SIZE> {
    fn new<F>(f: F) -> Self
    where
        F: FnMut(usize) -> T,
    {
        let inner: [T; SIZE] = from_fn(f);
        Self {
            inner: UnsafeCell::new(Box::new(inner)),
        }
    }

    fn inner(&self) -> &mut [T; SIZE] {
        unsafe { &mut *self.inner.get() }
    }
}

impl<T, const SIZE: usize> std::ops::Deref for UnsafeArr<T, SIZE> {
    type Target = [T; SIZE];
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.inner.get() }
    }
}

impl<T, const SIZE: usize> std::ops::DerefMut for UnsafeArr<T, SIZE> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.inner.get() }
    }
}

fn test_decrement_retest(sum: &AtomicIsize, delta: usize, ordering: Ordering) -> bool {
    let delta = delta as isize;
    let mut tdr = false;
    if sum.load(Relaxed) - delta >= 0 {
        if sum.fetch_sub(delta, ordering) >= delta {
            tdr = true;
        } else {
            sum.fetch_add(delta, Relaxed);
        }
    }

    tdr
}

fn test_increment_retest(
    sum: &AtomicIsize,
    delta: usize,
    bound: usize,
    ordering: Ordering,
) -> bool {
    let delta = delta as isize;
    let mut tir = false;
    if sum.load(Relaxed) + delta <= bound as isize {
        if sum.fetch_add(delta, ordering) + delta <= bound as isize {
            tir = true;
        } else {
            sum.fetch_sub(delta, Relaxed);
        }
    }

    tir
}

#[derive(Debug)]
struct Semaphore {
    permits: AtomicIsize,
    bound: usize,
}

impl Semaphore {
    fn new(available_permits: usize, bound: usize) -> Self {
        Self {
            permits: AtomicIsize::new(available_permits as isize),
            bound,
        }
    }

    fn acquire_permits(&self, n: usize) -> Result<(), SemaphoreErr> {
        assert!(n <= self.bound);
        while !test_decrement_retest(&self.permits, n, Acquire) {
            std::hint::spin_loop();
        }
        Ok(())
    }

    fn add_permits(&self, n: usize) -> Result<(), SemaphoreErr> {
        test_increment_retest(&self.permits, n, self.bound, Release);
        Ok(())
    }
}

#[derive(Debug)]
enum SemaphoreErr {}

fn pv_test_expanded(s: AtomicIsize, critical_section: impl Fn()) {
    'cycle_thread: loop {
        let mut tdr = false;
        while !tdr {
            if s.load(Relaxed) - 1 >= 0 {
                if s.fetch_sub(1, Acquire) >= 1 {
                    tdr = true;
                } else {
                    s.fetch_add(1, Relaxed);
                }
            }
        }
        critical_section();
        s.fetch_add(1, Release);
    }
}
