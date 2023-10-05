#![cfg_attr(not(feature = "std"), no_std)]

use core::fmt;
use core::future::Future;
use core::marker::PhantomData;
use core::task::{Context, Poll};

extern crate alloc;
use alloc::sync::Arc;

pub use async_task::{Runnable, Task};

#[cfg(feature = "std")]
pub use crate::std::*;

#[cfg(all(feature = "alloc", target_has_atomic = "ptr", target_has_atomic = "8"))]
pub use crate::eventloop::*;

#[cfg(all(feature = "alloc", feature = "esp-idf-hal", target_has_atomic = "ptr"))]
pub use crate::espidf::*;

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

/// This trait captures the notion of an executor wakeup
/// that needs to happen once a waker is awoken using `Waker::wake()`.
///
/// The implementation of the trait is operating system and
/// execution approach-specific. Some examples:
/// - On most operating systems with STD support, the `Monitor` implementation
///   is a (Mutex, Condvar) pair, which allows the executor to block/sleep on the mutex
///   when there there are no tasks pending for execution, and to be awoken
///   (via the condvar), when `wake()` is called on some waker.
/// - For event-loop based execution, the monitor implementation typically schedules
///   the executor to run the currently scheduled tasks on the event loop.
///   When there are no tasks scheduled for execution, the executor does not "sleep"
///   per-se, as it does not have a dedicated thread. Yet, it simply doesn't get scheduled
///   for execution on the event-loop.
pub trait Monitor {
    type Notify: Notify;

    fn notifier(&self) -> Self::Notify;
}

/// The `Monitor` trait is also essentially a factory
/// producing - on demand - instances of this trait.
///
/// This trait is performing the actual scheduling of the executor for execution
/// - as in by e.g. awaking the thread of the executor, or e.g. scheduling the executor on an event loop.
pub trait Notify {
    fn notify(&self);
}

/// Monitors that provide a "sleep" facility to the executor need to also
/// implement this trait. Most monitors should typically implement it, which enables
/// the `Executor::run` method.
///
/// What `Executor::run` does is to - in a loop - call `wait()` on the monitor.
/// Once awoken from `wait()` (which means a waker was awoken via a call to `Notifier::notify()`),
/// the executor runs all tasks which had been scheduled to run by their awoken wakers.
/// Once the scheduled tasks' queue is empty again, the executor calls `wait()` again.
///
/// Notable exceptions are event-loop based monitors like WASM and others
/// where the executor is started once (via `Executor::start`) and then
/// scheduled for execution on the event loop when notified.
pub trait Wait: Monitor {
    fn wait(&self);
}

/// An alternative to `Wait` that is typically implemented for executors that
/// are scheduled on an event-loop and thus do NOT follow the
/// "sleep the executor current thread until notified and then run the executor in the thread"
/// pattern, achieved by implementing the `Wait` trait.
///
/// Enables the `Executor::start` method.
pub trait Start: Monitor {
    fn start<P>(&self, poll_fn: P)
    where
        P: FnMut() + 'static;
}

/// Designates a `Local` executor.
///
/// Local executors can be polled and thus execute their tasks from a single thread only,
/// so task parallelism is not possible.
///
/// Yet, these executors have the benefit that futures spawned on them do not need to be `Send + 'static`
/// and can live only as long as the executor lives.
pub type Local = *const ();

/// Designates a `Sendable` (i.e. work-stealing) executor.
///
/// Sendable executors can be polled from multiple threads and thus can execute their tasks
/// in a parallel fashion from multiple threads, thus achieving task parallelism.
///
/// These executors require that the futures spawned on them are `Send + 'static`, as the futures
/// might be moved to other threads different from the thread where they were spawned.
pub type Sendable = ();

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
    /// Creates a new executor instance using the provided `Monitor` type.
    /// The monitor type needs to implement `Default`.
    pub fn new() -> Self
    where
        M: Default,
    {
        Self::wrap(Default::default())
    }

    /// Creates a new executor instance using the provided `Monitor` instance.
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

    /// Spawns a new, detached task for the supplied future.
    ///
    /// Note that if the executor's queue size is equal to the number of currently
    /// spawned and running tasks, spawning this additional task might cause the executor to panic
    /// later, when the task is scheduled for polling.
    pub fn spawn_detached<F, T>(&self, fut: F) -> Result<&Self, SpawnError>
    where
        F: Future<Output = T> + Send + 'a,
        T: 'a,
    {
        self.spawn(fut)?.detach();

        Ok(self)
    }

    /// Spawns a new task for the supplied future and collects it in the supplied `heapless::Vec` instance.
    ///
    /// Note that if the executor's queue size is equal to the number of currently
    /// spawned and running tasks, spawning this additional task might cause the executor to panic
    /// later, when the task is scheduled for polling.
    pub fn spawn_collect<F, T>(
        &self,
        fut: F,
        collector: &mut heapless::Vec<Task<T>, C>,
    ) -> Result<&Self, SpawnError>
    where
        F: Future<Output = T> + Send + 'a,
        T: 'a,
    {
        if collector.len() < C {
            let task = self.spawn(fut)?;

            collector
                .push(task)
                .map_err(|_| SpawnError::CollectorFull)?;

            Ok(self)
        } else {
            Err(SpawnError::CollectorFull)
        }
    }

    /// Spawns a new task for the supplied future.
    ///
    /// Note that if the executor's queue size is equal to the number of currently
    /// spawned and running tasks, spawning this additional task might cause the executor to panic
    /// later, when the task is scheduled for polling.
    pub fn spawn<F, T>(&self, fut: F) -> Result<Task<T>, SpawnError>
    where
        F: Future<Output = T> + Send + 'a,
        T: 'a,
    {
        unsafe { self.spawn_unchecked(fut) }
    }

    /// Unsafely spawns a new task for the supplied future.
    ///
    /// This method is unsafe because it is not checking whether the future lives as long
    /// as the executor.
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

    /// Pops the first task scheduled for execution by the executor.
    ///
    /// Typically useful when the executor works in a work-stealing mode,
    /// i.e. tasks are executed on a thread pool.
    ///
    /// For single-threaded execution, `poll`, `poll_one` are easier alternatives as they
    /// abstract popping and running the scheduled tasks.
    ///
    /// Returns
    /// - `None` - if no task was scheduled for execution
    /// - `Some(Runnnable)` - the first task scheduled for execution. Calling `Runnable::run` will
    ///    execute the task. In other words, it will poll its future.
    pub fn pop_scheduled(&self) -> Option<Runnable> {
        let runnable;

        #[cfg(feature = "crossbeam-queue")]
        {
            runnable = self.queue.pop();
        }

        #[cfg(not(feature = "crossbeam-queue"))]
        {
            runnable = self.queue.dequeue();
        }

        runnable
    }

    /// Polls the executor once and thus runs one task from those which had been scheduled to run by their wakers.
    ///
    /// Returns
    /// `Poll::Pending` if no tasks had been scheduled for execution.
    /// `Poll::Ready<()>` if at least one task was scheduled for execution and executed.
    pub fn poll_one(&self) -> Poll<()> {
        if let Some(runnable) = self.pop_scheduled() {
            runnable.run();

            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    /// Polls the executor and runs all tasks which had been scheduled to run by their wakers.
    /// Stops polling once `condition` becomes `false` OR when there are no more tasks scheduled for execution in the queue.
    ///
    /// Returns
    /// `Poll::Pending` when all tasks (if any) in the queue had been executed
    /// `Poll::Ready<()>` if `condition` became `false` in the meantime
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

    /// Drops all supplied tasks. This method is necessary, because the tasks can only be dropped while the executor is running.
    ///
    /// As a side effect, tasks which had not been supplied for dropping will also run.
    pub fn drop_tasks<T>(&self, tasks: T) {
        drop(tasks);

        while !self.poll_one().is_pending() {}
    }

    /// Starts the executor in the background and runs it until `condition` holds `true`.
    /// At the end of the execution, calls the `finished` callback.
    ///
    /// This method requires that the `Monitor` instance used by the executor implements the `Start` trait.
    /// This trait is typically provided by monitors which allow embedding the executor in an event loop,
    /// similar to the browser/WASM event loop.
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

    /// Continuously runs the executor while `condition` holds `true`.
    ///
    /// This method requires that the `Monitor` instance used by the executor implements the `Wait` trait.
    /// In other words, the monitor should be capable of providing "sleep" functionality to the executor.
    pub fn run<B>(&self, condition: B)
    where
        M: Wait,
        B: Fn() -> bool,
    {
        while self.poll(&condition).is_pending() {
            self.wait();
        }
    }

    /// Continuously runs the executor while `condition` holds `true`.
    /// Once `condition` becomes `false`, drops all supplied tasks.
    ///
    /// This method requires that the `Monitor` instance used by the executor implements the `Wait` trait.
    /// In other words, the monitor should be capable of providing "sleep" functionality to the executor.
    pub fn run_tasks<B, T>(&self, condition: B, tasks: T)
    where
        M: Wait,
        B: Fn() -> bool,
    {
        self.run(condition);

        self.drop_tasks(tasks)
    }

    /// Waits for one or more tasks to be scheduled for execution.
    ///
    /// This method requires that the `Monitor` instance used by the executor implements the `Wait` trait.
    /// In other words, the monitor should be capable of providing "sleep" functionality to the executor.
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
    /// Spawns a new, detached task for the supplied future.
    ///
    /// Note that if the executor's queue size is equal to the number of currently
    /// spawned and running tasks, spawning this additional task might cause the executor to panic
    /// later, when the task is scheduled for polling.
    pub fn spawn_local_detached<F, T>(&self, fut: F) -> Result<&Self, SpawnError>
    where
        F: Future<Output = T> + 'a,
        T: 'a,
    {
        self.spawn_local(fut)?.detach();

        Ok(self)
    }

    /// Spawns a new task for the supplied future and collects it in the supplied `heapless::Vec` instance.
    ///
    /// Note that if the executor's queue size is equal to the number of currently
    /// spawned and running tasks, spawning this additional task might cause the executor to panic
    /// later, when the task is scheduled for polling.
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

    /// Spawns a new task for the supplied future.
    ///
    /// Note that if the executor's queue size is equal to the number of currently
    /// spawned and running tasks, spawning this additional task might cause the executor to panic
    /// later, when the task is scheduled for polling.
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

/// A very simple executor that can execute - on the current thread - a single future.
#[derive(Clone)]
pub struct Blocker<M>(M);

impl<M> Blocker<M>
where
    M: Monitor + Wait,
    M::Notify: Send + Sync + 'static,
{
    /// Creates a new executor instance using the provided `Monitor` type.
    /// The monitor type needs to implement `Default`.
    pub fn new() -> Self
    where
        M: Default,
    {
        Self(Default::default())
    }

    /// Creates a new executor instance using the provided `Monitor` instance.
    pub const fn wrap(notify_factory: M) -> Self {
        Self(notify_factory)
    }

    /// Executes the supplied future on the current thread, this blocking it until the future becomes ready.
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

    /// A `Monitor` instance based on `std::sync::Mutex` and `std::sync::Condvar`
    /// Suitable for STD-compatible platforms where awaking a waker from an ISR
    /// is either not an option (regular operating systems like Linux)
    /// or not necessary.
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

    /// A `Monitor` instance for web-assembly (WASM) based browser targets.
    ///
    /// Works by integrating the monitor (and thus the executor) into the browser event loop.
    ///
    /// Tasks are scheduled for execution in the browser event loop, by turning those into JavaScript Promises.
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

    unsafe impl Send for EventLoopContext {}
    unsafe impl Sync for EventLoopContext {}

    /// A generic `Monitor` instance useful for integrating into a native event loop, by scheduling
    /// the execution of the tasks to happen in the event loop.
    ///
    /// Only event loops that provide a way to schedule a piece of "work" in the event loop are
    /// amenable to such integration. Typical event loops include the GLIB event loop, the Matter
    /// C++ SDK event loop, and possibly many others.
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

#[cfg(all(feature = "alloc", feature = "esp-idf-hal", target_has_atomic = "ptr"))]
mod espidf {
    use core::num::NonZeroU32;

    use esp_idf_hal::task::notification;

    pub use super::*;

    pub type EspExecutor<'a, const C: usize, S> = Executor<'a, C, FreeRtosMonitor, S>;
    pub type EspBlocker = Blocker<FreeRtosMonitor>;

    pub type FreeRtosMonitor = notification::Monitor;
    pub type FreeRtosNotify = notification::Notifier;

    impl Monitor for notification::Monitor {
        type Notify = notification::Notifier;

        fn notifier(&self) -> Self::Notify {
            notification::Monitor::notifier(self)
        }
    }

    impl Wait for notification::Monitor {
        fn wait(&self) {
            notification::Monitor::wait_any(self)
        }
    }

    impl Notify for notification::Notifier {
        fn notify(&self) {
            unsafe {
                notification::Notifier::notify_and_yield(self, NonZeroU32::new(1).unwrap());
            }
        }
    }
}
