#![cfg_attr(not(feature = "std"), no_std)]

use core::fmt;
use core::future::Future;
use core::marker::PhantomData;
use core::task::{Context, Poll};

extern crate alloc;
use alloc::sync::Arc;

use async_task::{Runnable, Task};

use heapless::mpmc::MpMcQueue;

#[cfg(feature = "std")]
pub use crate::std::*;

#[derive(Debug)]
pub enum SpawnError {
    QueueFull,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Queue Full Error")
    }
}

#[cfg(feature = "std")]
impl ::std::error::Error for SpawnError {}

pub trait Wait {
    fn wait(&self);
}

impl<F> Wait for F
where
    F: Fn(),
{
    fn wait(&self) {
        (self)()
    }
}

pub trait Notify: Send + Sync {
    fn notify(&self);
}

impl<F> Notify for F
where
    F: Fn() + Send + Sync,
{
    fn notify(&self) {
        (self)()
    }
}

pub trait NotifyFactory {
    type Notify: Notify;

    fn notifier(&self) -> Self::Notify;
}

pub trait RunContextFactory {
    fn prerun(&self) {}
    fn postrun(&self) {}
}

pub struct RunContext(());

pub type Local = *const ();
pub type Sendable = ();

/// WORK IN PROGRESS
/// `Executor` is an async executor that is useful specfically for embedded environments.
///
/// The implementation is in fact a thin wrapper around [smol](::smol)'s [async-task](::async-task) crate.
///
/// Highlights:
/// - `no_std` (but does need `alloc`; for a `no_std` *and* "no_alloc" executor, look at [Embassy](::embassy), which statically pre-allocates all tasks);
///            (note also that usage of `alloc` is very limited - only when a new task is being spawn, as well as the executor itself);
/// - Does not assume an RTOS and can run completely bare-metal (or on top of an RTOS);
/// - Pluggable [Wait] & [Notify] mechanism which makes it ISR-friendly. In particular:
///   - Tasks can be woken up (and thus re-scheduled) from within an ISR;
///   - The executor itself can run not just in the main "thread", but from within an ISR as well;
///     the latter is important for bare-metal non-RTOS use-cases, as it allows - by running multiple executors -
///     one in the main "thread" and the others - on ISR interrupts - to achieve RTOS-like pre-emptive execution by scheduling higher-priority
///     tasks in executors that run on (higher-level) ISRs and thus pre-empt the executors
///     scheduled on lower-level ISR and the "main thread" one; note that deploying an executor in an ISR requires the allocator to be usable
///     from an ISR (i.e. allocation/deallocation routines should be protected by critical sections that disable/enable interrupts);
///   - Out of the box implementations for [Wait] & [Notify] based on condvars, compatible with Rust STD
///     (for cases where notifying from / running in ISRs is not important).
pub struct Executor<'a, const C: usize, N, W, S = Sendable> {
    queue: Arc<MpMcQueue<Runnable, C>>,
    notify_factory: N,
    wait: W,
    _sendable: PhantomData<S>,
    _marker: PhantomData<core::cell::UnsafeCell<&'a ()>>,
}

#[allow(clippy::missing_safety_doc)]
impl<'a, const C: usize, N, W, S> Executor<'a, C, N, W, S>
where
    N: NotifyFactory + RunContextFactory,
    W: Wait,
{
    pub fn new(notify_factory: N, wait: W) -> Self {
        Self {
            queue: Arc::new(MpMcQueue::<_, C>::new()),
            notify_factory,
            wait,
            _sendable: PhantomData,
            _marker: PhantomData,
        }
    }

    pub fn spawn_detached<F, T>(&mut self, fut: F) -> Result<&mut Self, SpawnError>
    where
        F: Future<Output = T> + Send + 'a,
        T: 'a,
    {
        self.spawn(fut)?.detach();

        Ok(self)
    }

    pub fn spawn<F, T>(&mut self, fut: F) -> Result<Task<T>, SpawnError>
    where
        F: Future<Output = T> + Send + 'a,
        T: 'a,
    {
        unsafe { self.spawn_unchecked(fut) }
    }

    pub unsafe fn spawn_unchecked<F, T>(&mut self, fut: F) -> Result<Task<T>, SpawnError>
    where
        F: Future<Output = T>,
    {
        let schedule = {
            let queue = self.queue.clone();
            let notify = self.notify_factory.notifier();

            move |runnable| {
                queue.enqueue(runnable).unwrap();
                notify.notify();
            }
        };

        let (runnable, task) = async_task::spawn_unchecked(fut, schedule);

        runnable.schedule();

        Ok(task)
    }

    pub fn with_context<F, T>(&mut self, run: F) -> T
    where
        F: FnOnce(&mut Self, &RunContext) -> T,
    {
        self.notify_factory.prerun();

        let result = run(self, &RunContext(()));

        self.notify_factory.postrun();

        result
    }

    pub fn tick(&mut self, _context: &RunContext) -> bool {
        if let Some(runnable) = self.queue.dequeue() {
            runnable.run();

            true
        } else {
            false
        }
    }

    pub fn wait(&mut self, _context: &RunContext) {
        self.wait.wait();
    }
}

impl<'a, const C: usize, N, W> Executor<'a, C, N, W, Local>
where
    N: NotifyFactory + RunContextFactory,
    W: Wait,
{
    pub fn spawn_local_detached<F, T>(&mut self, fut: F) -> Result<&mut Self, SpawnError>
    where
        F: Future<Output = T> + 'a,
        T: 'a,
    {
        self.spawn_local(fut)?.detach();

        Ok(self)
    }

    pub fn spawn_local<F, T>(&mut self, fut: F) -> Result<Task<T>, SpawnError>
    where
        F: Future<Output = T> + 'a,
        T: 'a,
    {
        unsafe { self.spawn_unchecked(fut) }
    }
}

#[derive(Clone)]
pub struct EmbeddedBlocker<N, W>(N, W);

impl<N, W> EmbeddedBlocker<N, W>
where
    N: NotifyFactory,
    N::Notify: 'static,
    W: Wait,
{
    pub const fn new(notify_factory: N, wait: W) -> Self {
        Self(notify_factory, wait)
    }

    pub fn block_on<F>(&self, mut f: F) -> F::Output
    where
        F: Future,
    {
        log::trace!("block_on(): started");

        let mut f = unsafe { core::pin::Pin::new_unchecked(&mut f) };

        let notify = self.0.notifier();

        let waker = waker_fn::waker_fn(move || {
            notify.notify();
        });

        let cx = &mut Context::from_waker(&waker);

        loop {
            if let Poll::Ready(t) = f.as_mut().poll(cx) {
                log::trace!("block_on(): completed");
                return t;
            }

            self.1.wait();
        }
    }
}

#[cfg(feature = "embedded-svc")]
impl<N, W> embedded_svc::executor::asynch::Blocker for EmbeddedBlocker<N, W>
where
    N: NotifyFactory,
    N::Notify: 'static,
    W: Wait,
{
    fn block_on<F>(&self, f: F) -> F::Output
    where
        F: Future,
    {
        EmbeddedBlocker::block_on(self, f)
    }
}

#[cfg(feature = "std")]
mod std {
    use std::sync::{Condvar, Mutex};

    extern crate alloc;
    use alloc::{rc::Rc, sync::Arc};

    use crate::{Notify, NotifyFactory, RunContextFactory, Wait};

    #[derive(Clone)]
    pub struct StdWait(Rc<Mutex<()>>, Arc<Condvar>);

    impl StdWait {
        pub fn new() -> Self {
            Self(Rc::new(Mutex::new(())), Arc::new(Condvar::new()))
        }

        pub fn notify_factory(&self) -> Arc<Condvar> {
            self.1.clone()
        }
    }

    impl Default for StdWait {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Wait for StdWait {
        fn wait(&self) {
            let guard = self.0.lock().unwrap();

            let _ = self.1.wait(guard).unwrap();
        }
    }

    pub type StdNotifyFactory = Arc<Condvar>;

    impl NotifyFactory for StdNotifyFactory {
        type Notify = Self;

        fn notifier(&self) -> Self::Notify {
            self.clone()
        }
    }

    impl RunContextFactory for StdNotifyFactory {}

    impl Notify for StdNotifyFactory {
        fn notify(&self) {
            self.notify_one();
        }
    }
}