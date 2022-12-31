#![cfg_attr(not(feature = "std"), no_std)]

use core::fmt;
use core::future::Future;
use core::marker::PhantomData;
use core::task::{Context, Poll};

extern crate alloc;
use alloc::sync::Arc;

use async_task::Runnable;

pub use async_task::Task;

#[cfg(feature = "std")]
pub use crate::std::*;

#[cfg(all(feature = "alloc", target_has_atomic = "ptr", target_has_atomic = "8"))]
pub use crate::eventloop::*;

#[cfg(feature = "wasm")]
pub use crate::wasm::*;

#[derive(Debug)]
pub enum SpawnError {
    QueueFull,
    CollectorFull,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueFull => write!(f, "Queue Full Error"),
            Self::CollectorFull => write!(f, "Collector Full Error"),
        }
    }
}

#[cfg(feature = "std")]
impl ::std::error::Error for SpawnError {}

pub trait Notify {
    fn notify(&self);
}

pub trait Monitor {
    type Notify: Notify;

    fn notifier(&self) -> Self::Notify;
}

pub trait Wait {
    fn wait(&self);
}

pub trait Start {
    fn start<P>(&self, poll_fn: P)
    where
        P: FnMut() + 'static;
}

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
/// - Pluggable [Monitor] mechanism which makes it ISR-friendly. In particular:
///   - Tasks can be woken up (and thus re-scheduled) from within an ISR;
///   - The executor itself can run not just in the main "thread", but from within an ISR as well;
///     the latter is important for bare-metal non-RTOS use-cases, as it allows - by running multiple executors -
///     one in the main "thread" and the others - on ISR interrupts - to achieve RTOS-like pre-emptive execution by scheduling higher-priority
///     tasks in executors that run on (higher-level) ISRs and thus pre-empt the executors
///     scheduled on lower-level ISR and the "main thread" one; note that deploying an executor in an ISR requires the allocator to be usable
///     from an ISR (i.e. allocation/deallocation routines should be protected by critical sections that disable/enable interrupts);
///   - Out of the box implementation for [Monitor] based on condvar, compatible with Rust STD
///     (for cases where notifying from / running in ISRs is not important).
///   - Out of the box implementation for [Monitor] based on WASM event loop, compatible with WASM
pub struct Executor<'a, const C: usize, M, S = Local> {
    #[cfg(feature = "crossbeam-queue")]
    queue: Arc<crossbeam_queue::ArrayQueue<Runnable>>,
    #[cfg(not(feature = "crossbeam-queue"))]
    queue: Arc<heapless::mpmc::MpMcQueue<Runnable, C>>,
    monitor: M,
    _sendable: PhantomData<S>,
    _marker: PhantomData<core::cell::UnsafeCell<&'a ()>>,
}

#[allow(clippy::missing_safety_doc)]
impl<'a, const C: usize, M, S> Executor<'a, C, M, S>
where
    M: Monitor,
{
    pub fn new() -> Self
    where
        M: Default,
    {
        Self::wrap(Default::default())
    }

    pub fn wrap(monitor: M) -> Self {
        Self {
            #[cfg(feature = "crossbeam-queue")]
            queue: Arc::new(crossbeam_queue::ArrayQueue::new(C)),
            #[cfg(not(feature = "crossbeam-queue"))]
            queue: Arc::new(heapless::mpmc::MpMcQueue::<_, C>::new()),
            monitor,
            _sendable: PhantomData,
            _marker: PhantomData,
        }
    }

    pub fn spawn_detached<F, T>(&self, fut: F) -> Result<&Self, SpawnError>
    where
        F: Future<Output = T> + Send + 'a,
        T: 'a,
    {
        self.spawn(fut)?.detach();

        Ok(self)
    }

    pub fn spawn_collect<F, T>(
        &self,
        fut: F,
        collector: &mut heapless::Vec<Task<T>, C>,
    ) -> Result<&Self, SpawnError>
    where
        F: Future<Output = T> + Send + 'a,
        T: 'a,
    {
        let task = self.spawn(fut)?;

        collector
            .push(task)
            .map_err(|_| SpawnError::CollectorFull)?;

        Ok(self)
    }

    pub fn spawn<F, T>(&self, fut: F) -> Result<Task<T>, SpawnError>
    where
        F: Future<Output = T> + Send + 'a,
        T: 'a,
    {
        unsafe { self.spawn_unchecked(fut) }
    }

    pub unsafe fn spawn_unchecked<F, T>(&self, fut: F) -> Result<Task<T>, SpawnError>
    where
        F: Future<Output = T>,
    {
        let schedule = {
            let queue = self.queue.clone();
            let notify = self.monitor.notifier();

            move |runnable| {
                #[cfg(feature = "crossbeam-queue")]
                {
                    queue.push(runnable).unwrap();
                }

                #[cfg(not(feature = "crossbeam-queue"))]
                {
                    queue.enqueue(runnable).unwrap();
                }

                notify.notify();
            }
        };

        let (runnable, task) = async_task::spawn_unchecked(fut, schedule);

        runnable.schedule();

        Ok(task)
    }

    pub fn poll_one(&self) -> Poll<()> {
        let runnable;

        #[cfg(feature = "crossbeam-queue")]
        {
            runnable = self.queue.pop();
        }

        #[cfg(not(feature = "crossbeam-queue"))]
        {
            runnable = self.queue.dequeue();
        }

        if let Some(runnable) = runnable {
            runnable.run();

            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    pub fn poll<B>(&self, condition: B) -> Poll<()>
    where
        B: Fn() -> bool,
    {
        while condition() {
            if self.poll_one().is_pending() {
                return Poll::Pending;
            }
        }

        Poll::Ready(())
    }

    pub fn drop_tasks<T>(&self, tasks: T) {
        drop(tasks);

        while !self.poll_one().is_pending() {}
    }

    // Method below only works when a `Start` implementation is provided
    // (i.e. the executor runs in the background, in an ISR context or in the WASM event loop)

    pub fn start<B, F>(&'static self, condition: B, finished: F)
    where
        M: Start,
        B: Fn() -> bool + 'static,
        F: FnOnce() + 'static,
    {
        let executor = self;
        let mut finished_opt = Some(finished);

        self.monitor.start(move || {
            if executor.poll(&condition).is_ready() {
                if let Some(finished) = finished_opt.take() {
                    finished();
                } else {
                    unreachable!("Should not happen - finished called twice");
                }
            }
        });
    }

    // Methods below only work when a `Wait` implementation is provided
    // (i.e. the executor runs in a thread)

    pub fn run<B>(&self, condition: B)
    where
        M: Wait,
        B: Fn() -> bool,
    {
        while self.poll(&condition).is_pending() {
            self.wait();
        }
    }

    pub fn run_tasks<B, T>(&self, condition: B, tasks: T)
    where
        M: Wait,
        B: Fn() -> bool,
    {
        self.run(condition);

        self.drop_tasks(tasks)
    }

    pub fn wait(&self)
    where
        M: Wait,
    {
        self.monitor.wait();
    }
}

impl<'a, const C: usize, M> Executor<'a, C, M, Local>
where
    M: Monitor,
{
    pub fn spawn_local_detached<F, T>(&self, fut: F) -> Result<&Self, SpawnError>
    where
        F: Future<Output = T> + 'a,
        T: 'a,
    {
        self.spawn_local(fut)?.detach();

        Ok(self)
    }

    pub fn spawn_local_collect<F, T>(
        &self,
        fut: F,
        collector: &mut heapless::Vec<Task<T>, C>,
    ) -> Result<&Self, SpawnError>
    where
        F: Future<Output = T> + 'a,
        T: 'a,
    {
        let task = self.spawn_local(fut)?;

        collector
            .push(task)
            .map_err(|_| SpawnError::CollectorFull)?;

        Ok(self)
    }

    pub fn spawn_local<F, T>(&self, fut: F) -> Result<Task<T>, SpawnError>
    where
        F: Future<Output = T> + 'a,
        T: 'a,
    {
        unsafe { self.spawn_unchecked(fut) }
    }
}

impl<'a, const C: usize, M, S> Default for Executor<'a, C, M, S>
where
    M: Monitor + Default,
{
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct Blocker<M>(M);

impl<M> Blocker<M>
where
    M: Monitor + Wait,
    M::Notify: Send + Sync + 'static,
{
    pub fn new() -> Self
    where
        M: Default,
    {
        Self(Default::default())
    }

    pub const fn wrap(notify_factory: M) -> Self {
        Self(notify_factory)
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

            self.0.wait();
        }
    }
}

impl<M> Default for Blocker<M>
where
    M: Monitor + Wait + Default,
    M::Notify: Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

// #[cfg(feature = "embedded-svc")]
// impl<M> embedded_svc::executor::asynch::Blocker for Blocker<M>
// where
//     M: Monitor + Wait,
//     M::Notify: Send + Sync + 'static,
// {
//     fn block_on<F>(&self, f: F) -> F::Output
//     where
//         F: Future,
//     {
//         Blocker::block_on(self, f)
//     }
// }

#[cfg(feature = "std")]
mod std {
    use std::sync::{Condvar, Mutex};

    extern crate alloc;
    use alloc::sync::{Arc, Weak};

    use crate::{Monitor, Notify, Wait};

    pub struct StdMonitor(Mutex<()>, Arc<Condvar>);

    impl StdMonitor {
        pub fn new() -> Self {
            Self(Mutex::new(()), Arc::new(Condvar::new()))
        }
    }

    impl Default for StdMonitor {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Monitor for StdMonitor {
        type Notify = StdNotify;

        fn notifier(&self) -> Self::Notify {
            StdNotify(Arc::downgrade(&self.1))
        }
    }

    impl Wait for StdMonitor {
        fn wait(&self) {
            let guard = self.0.lock().unwrap();

            drop(self.1.wait(guard).unwrap());
        }
    }

    pub struct StdNotify(Weak<Condvar>);

    impl Notify for StdNotify {
        fn notify(&self) {
            if let Some(notify) = Weak::upgrade(&self.0) {
                notify.notify_one();
            }
        }
    }
}

#[cfg(feature = "wasm")]
mod wasm {
    use core::cell::RefCell;

    extern crate alloc;
    use alloc::rc::{Rc, Weak};

    use js_sys::Promise;

    use wasm_bindgen::prelude::*;

    use crate::{Monitor, Notify, Start};

    struct WasmContext {
        promise: Promise,
        closure: Option<Closure<dyn FnMut(JsValue)>>,
    }

    pub struct WasmMonitor(Rc<RefCell<WasmContext>>);

    impl WasmMonitor {
        pub fn new() -> Self {
            Self(Rc::new(RefCell::new(WasmContext {
                promise: Promise::resolve(&JsValue::undefined()),
                closure: None,
            })))
        }
    }

    impl Default for WasmMonitor {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Monitor for WasmMonitor {
        type Notify = WasmNotify;

        fn notifier(&self) -> Self::Notify {
            WasmNotify(Rc::downgrade(&self.0))
        }
    }

    impl Start for WasmMonitor {
        fn start<P>(&self, mut poll_fn: P)
        where
            P: FnMut() + 'static,
        {
            {
                self.0.borrow_mut().closure = Some(Closure::new(move |_| poll_fn()));
            }

            self.notifier().notify();
        }
    }

    pub struct WasmNotify(Weak<RefCell<WasmContext>>);

    impl Notify for WasmNotify {
        fn notify(&self) {
            if let Some(rc) = Weak::upgrade(&self.0) {
                let ctx = rc.borrow_mut();

                if let Some(closure) = ctx.closure.as_ref() {
                    let _ = ctx.promise.then(closure);
                }
            }
        }
    }
}

#[cfg(all(feature = "alloc", target_has_atomic = "ptr", target_has_atomic = "8"))]
mod eventloop {
    use core::cell::UnsafeCell;
    use core::marker::PhantomData;
    use core::sync::atomic::{AtomicBool, Ordering};

    extern crate alloc;
    use alloc::boxed::Box;
    use alloc::sync::{Arc, Weak};

    use crate::{Monitor, Notify, Start};

    struct EventLoopContext {
        scheduler: Box<dyn Fn(extern "C" fn(*mut ()), *mut ())>,
        scheduled: AtomicBool,
        poller: UnsafeCell<Option<Box<dyn FnMut()>>>,
    }

    pub struct EventLoopMonitor(Arc<EventLoopContext>, PhantomData<*const ()>);

    impl EventLoopMonitor {
        pub fn new<S>(scheduler: S) -> Self
        where
            S: Fn(extern "C" fn(*mut ()), *mut ()) + 'static,
        {
            Self(
                Arc::new(EventLoopContext {
                    scheduler: Box::new(scheduler),
                    scheduled: AtomicBool::new(false),
                    poller: UnsafeCell::new(None),
                }),
                PhantomData,
            )
        }
    }

    impl Monitor for EventLoopMonitor {
        type Notify = EventLoopNotify;

        fn notifier(&self) -> Self::Notify {
            EventLoopNotify(Arc::downgrade(&self.0))
        }
    }

    impl Start for EventLoopMonitor {
        fn start<P>(&self, poll_fn: P)
        where
            P: FnMut() + 'static,
        {
            {
                *unsafe { self.0.poller.get().as_mut() }.unwrap() = Some(Box::new(poll_fn));
            }

            self.notifier().notify();
        }
    }

    pub struct EventLoopNotify(Weak<EventLoopContext>);

    impl EventLoopNotify {
        extern "C" fn run(ctx: *mut ()) {
            let ctx = unsafe { Arc::from_raw(ctx as *const EventLoopContext) };

            ctx.scheduled.store(false, Ordering::SeqCst);

            if let Some(poll_fn) = unsafe { ctx.poller.get().as_mut() }.unwrap().as_mut() {
                poll_fn();
            }
        }
    }

    impl Notify for EventLoopNotify {
        fn notify(&self) {
            if let Some(ctx) = Weak::upgrade(&self.0) {
                if let Ok(false) =
                    ctx.scheduled
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                {
                    // TODO: Will leak the Arc if the scheduled event is not executed
                    (ctx.scheduler)(Self::run, Arc::into_raw(ctx.clone()) as *mut _);
                }
            }
        }
    }
}
