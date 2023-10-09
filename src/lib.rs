#![cfg_attr(not(feature = "std"), no_std)]

use core::fmt;
use core::future::{poll_fn, Future};
use core::marker::PhantomData;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

extern crate alloc;

use alloc::rc::Rc;
use alloc::sync::Arc;
use alloc::task::Wake;

pub use async_task::{Runnable, Task};

use atomic_waker::AtomicWaker;

use futures_lite::FutureExt;

#[cfg(feature = "std")]
pub use crate::std::*;

#[cfg(all(target_has_atomic = "ptr", target_has_atomic = "8"))]
pub use crate::ceventloop::*;

#[cfg(target_os = "espidf")]
pub use crate::espidf::*;

#[cfg(target_arch = "wasm32")]
pub use crate::wasm::*;

/// `Wakeup` captures the notion of an execution wakeup that needs to happen
/// once a waker is awoken using `Waker::wake()`.
///
/// The implementation of the trait is operating system and
/// execution approach-specific. Some examples:
/// - On most operating systems with STD support, the `Wakeup` implementation
///   is a (Mutex, Condvar) pair, which allows the executor to block/sleep on the mutex
///   when there there are no tasks pending for execution, and to be awoken
///   (via the condvar), when `wake()` is called on some task `Waker`.
/// - For event-loop based execution, the monitor implementation typically schedules
///   the executor to run the currently scheduled tasks on the event loop.
///   When there are no tasks scheduled for execution, the executor does not "sleep"
///   per-se, as it does not have a dedicated thread. Yet, it simply doesn't get scheduled
///   for execution on the event-loop.
pub trait Wakeup {
    type Wake: Wake + Send + Sync + 'static;

    fn waker(&self) -> Arc<Self::Wake>;
}

impl<T> Wakeup for &T
where
    T: Wakeup,
{
    type Wake = T::Wake;

    fn waker(&self) -> Arc<Self::Wake> {
        (*self).waker()
    }
}

impl<T> Wakeup for &mut T
where
    T: Wakeup,
{
    type Wake = T::Wake;

    fn waker(&self) -> Arc<Self::Wake> {
        (**self).waker()
    }
}

/// `Wait` is an extension of `Wakeup`.
/// `Wakeup` instances that provide a "sleep" facility need to also implement this trait.
/// Most `Wakeup` implementations should typically implement it.
///
/// What `Wait::run` does is to - in a loop - call `wait()` on the wakeup instance.
/// Once awoken from `wait()` (which means a task `Waker` was awoken and it scheduled its task and called `Wake::wake`),
/// the executor runs all tasks which had been scheduled to run by their awoken wakers.
/// Once the scheduled tasks' queue is empty again, the executor calls `wait()` again.
///
/// Notable exceptions are event-loop based monitors like WASM and others
/// where the executor is scheduled once and then
/// re-scheduled for execution on the event loop when awoken.
pub trait Wait: Wakeup {
    fn wait(&self);

    /// Executes the supplied future on the current thread, thus blocking it until the future becomes ready.
    fn block_on<F>(&self, mut fut: F) -> F::Output
    where
        Self: Wait,
        F: Future,
    {
        log::trace!("block_on(): started");

        let mut fut = pin!(fut);

        let waker = self.waker().into();

        let mut cx = Context::from_waker(&waker);

        let res = loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(res) => break res,
                Poll::Pending => self.wait(),
            }
        };

        log::trace!("block_on(): finished");

        res
    }
}

impl<T> Wait for &T
where
    T: Wait,
{
    fn wait(&self) {
        (*self).wait();
    }
}

impl<T> Wait for &mut T
where
    T: Wait,
{
    fn wait(&self) {
        (**self).wait();
    }
}

#[derive(Debug)]
pub enum ScheduleError {
    WrongContext,
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Schedule error")
    }
}

#[cfg(feature = "std")]
impl ::std::error::Error for ScheduleError {}

/// `Schedule` is an alternative to `Wait` that is typically implemented for executors that
/// are to be scheduled on an event-loop (or something similar to an event-loop) and thus do not follow the
/// "sleep the executor current thread until notified and then run the executor in the thread"
/// pattern, achieved by implementing the `Wait` trait.
pub trait Schedule: Wakeup {
    /// Schedules `poll_fn` to run at some later time, on the event-loop.
    ///
    /// Calling this method MUST be done from within the "event-loop" as well, or else
    /// the implementation will err with `ScheduleError`.
    fn schedule_poll_fn<P>(&self, poll_fn: P) -> Result<(), ScheduleError>
    where
        P: FnMut() -> bool + 'static;

    /// Schedules the future on the event-loop and runs it until it becomes ready.
    /// At the end of the execution, calls the `on_complete` callback.
    ///
    /// Calling this method MUST be done from within the "event-loop" as well, or else
    /// the implementation will err with `ScheduleError`.
    fn schedule<F, R>(&self, fut: F, on_complete: R) -> Result<(), ScheduleError>
    where
        F: Future + 'static,
        R: FnOnce(F::Output) + 'static,
    {
        log::trace!("schedule(): started");

        let mut fut = alloc::boxed::Box::pin(fut);
        let mut on_complete = Some(on_complete);
        let wake = self.waker();

        let poll_fn = move || {
            let waker = Waker::from(wake.clone());
            let mut cx = Context::from_waker(&waker);

            if let Some(on_complete) = on_complete.take() {
                match fut.as_mut().poll(&mut cx) {
                    Poll::Ready(res) => {
                        on_complete(res);

                        log::trace!("schedule(): finished");
                        true
                    }
                    Poll::Pending => false,
                }
            } else {
                true
            }
        };

        self.schedule_poll_fn(poll_fn)
    }
}

impl<T> Schedule for &T
where
    T: Schedule,
{
    fn schedule_poll_fn<P>(&self, poll_fn: P) -> Result<(), ScheduleError>
    where
        P: FnMut() -> bool + 'static,
    {
        (*self).schedule_poll_fn(poll_fn)
    }
}

impl<T> Schedule for &mut T
where
    T: Schedule,
{
    fn schedule_poll_fn<P>(&self, poll_fn: P) -> Result<(), ScheduleError>
    where
        P: FnMut() -> bool + 'static,
    {
        (**self).schedule_poll_fn(poll_fn)
    }
}

/// `LocalExecutor` is an async executor for microcontrollers.
///
/// The implementation is a thin wrapper around [smol](::smol)'s [async-task](::async-task) crate.
///
/// Highlights:
/// - `no_std` (but does need `alloc`; for a `no_std` *and* "no_alloc" executor, look at [Embassy](::embassy), which statically pre-allocates all tasks);
///            (note also that the executor uses allocations in a limited way: when a new task is being spawn, as well as the executor itself);
///
/// - Tries to follow closely the API of [smol](::smol)'s [async-executor](::async-executor) crate, so that it can serve as a (mostly) drop-in replacement;
///
/// - Does not assume an RTOS and can run completely bare-metal (or on top of an RTOS);
///
/// - Local execution only. No plans for implementing work-stealing execution, as threads are either a scarce resource on microcontrollers' RTOS,
///   or do not exist at all (Rust bare-metal);
///
/// - Pluggable [Wakeup] mechanism which makes it customizable for different microcontrollers;
///
/// - ISR-friendly, i.e. tasks can be woken up (and thus re-scheduled) from within an ISR
///   (Enable with feature `wake-from-isr`);
///
/// - [StdWakeup] implementation based on a mutex + condvar pair, compatible with Rust STD
///   (Enable with feature `std`; for cases where notifying from / running in ISRs is not important);
///
/// - [EspWakeup] implementation for ESP-IDF based on FreeRTOS task notifications, compatible with the `wake-from-isr` feature
///   (enable with feature `espidf`);
///
/// - [WasmWakeup] implementation for the WASM event loop, compatible with WASM
///   (enable with feature `wasm`);
///
/// - [CEventLoopWakeup] implementation for native event loops like those of GLIB, the Matter C++ SDK and others.
pub struct LocalExecutor<'a, W, const C: usize = 64> {
    #[cfg(feature = "unbounded")]
    queue: Arc<crossbeam_queue::SegQueue<Runnable>>,
    #[cfg(not(feature = "unbounded"))]
    queue: Arc<crossbeam_queue::ArrayQueue<Runnable>>,
    wakeup: W,
    poll_runnable_waker: AtomicWaker,
    _marker: PhantomData<core::cell::UnsafeCell<&'a Rc<()>>>,
}

#[allow(clippy::missing_safety_doc)]
impl<'a, const C: usize, W> LocalExecutor<'a, W, C>
where
    W: Wakeup,
{
    /// Creates a new executor instance using the provided `Wakeup` type.
    /// The wakeup type needs to implement `Default`.
    pub fn new() -> Self
    where
        W: Default,
    {
        Self::wrap(Default::default())
    }

    /// Creates a new executor instance using the provided `Wakeup` instance.
    pub fn wrap(wakeup: W) -> Self {
        Self {
            #[cfg(feature = "unbounded")]
            queue: Arc::new(crossbeam_queue::SegQueue::new()),
            #[cfg(not(feature = "unbounded"))]
            queue: Arc::new(crossbeam_queue::ArrayQueue::new(C)),
            wakeup,
            poll_runnable_waker: AtomicWaker::new(),
            _marker: PhantomData,
        }
    }

    /// Returns a reference to the `Wakeup` instance owned by this executor.
    pub fn wakeup(&self) -> &W {
        &self.wakeup
    }

    /// Spawns a task onto the executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use edge_executor::{LocalExecutor, PlatformWakeup};
    ///
    /// let local_ex = LocalExecutor::<PlatformWakeup>::new();
    ///
    /// let task = local_ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// ```
    ///
    /// Note that if the executor's queue size is equal to the number of currently
    /// spawned and running tasks, spawning this additional task might cause the executor to panic
    /// later, when the task is scheduled for polling.
    pub fn spawn<F>(&self, fut: F) -> Task<F::Output>
    where
        F: Future + 'a,
        F::Output: 'a,
    {
        let schedule = {
            let queue = self.queue.clone();
            let wake = self.wakeup.waker();

            move |runnable| {
                #[cfg(feature = "unbounded")]
                {
                    queue.push(runnable);
                }

                #[cfg(not(feature = "unbounded"))]
                {
                    queue.push(runnable).unwrap();
                }

                wake.wake_by_ref();
            }
        };

        let (runnable, task) = unsafe { async_task::spawn_unchecked(fut, schedule) };

        runnable.schedule();

        task
    }

    /// Pops the first task scheduled for execution by the executor.
    ///
    /// Returns
    /// - `None` - if no task was scheduled for execution
    /// - `Some(Runnnable)` - the first task scheduled for execution. Calling `Runnable::run` will
    ///    execute the task. In other words, it will poll its future.
    fn try_runnable(&self) -> Option<Runnable> {
        self.queue.pop()
    }

    /// Polls the first task scheduled for execution by the executor.
    fn poll_runnable(&self, ctx: &Context<'_>) -> Poll<Runnable> {
        self.poll_runnable_waker.register(ctx.waker());

        if let Some(runnable) = self.try_runnable() {
            Poll::Ready(runnable)
        } else {
            Poll::Pending
        }
    }

    /// Waits for the next runnable task to run.
    async fn runnable(&self) -> Runnable {
        poll_fn(|ctx| self.poll_runnable(ctx)).await
    }

    /// Attempts to run a task if at least one is scheduled.
    ///
    /// Running a scheduled task means simply polling its future once.
    ///
    /// # Examples
    ///
    /// ```
    /// use edge_executor::{LocalExecutor, PlatformWakeup};
    ///
    /// let ex = LocalExecutor::<PlatformWakeup>::new();
    /// assert!(!ex.try_tick()); // no tasks to run
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// assert!(ex.try_tick()); // a task was found
    /// ```    
    pub fn try_tick(&self) -> bool {
        if let Some(runnable) = self.try_runnable() {
            runnable.run();

            true
        } else {
            false
        }
    }

    /// Runs a single task asynchronously.
    ///
    /// Running a task means simply polling its future once.
    ///
    /// If no tasks are scheduled when this method is called, it will wait until one is scheduled.
    ///
    /// # Examples
    ///
    /// ```
    /// use edge_executor::{LocalExecutor, PlatformWakeup, Wait};
    ///
    /// let ex = LocalExecutor::<PlatformWakeup>::new();
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// ex.wakeup().block_on(ex.tick()); // runs the task
    /// ```
    pub async fn tick(&self) {
        self.runnable().await.run();
    }

    /// Runs the executor asynchronously until the given future completes.
    ///
    /// # Examples
    ///
    /// ```
    /// use edge_executor::{LocalExecutor, PlatformWakeup, Wait};
    ///
    /// let local_ex = LocalExecutor::<PlatformWakeup>::new();
    ///
    /// let task = local_ex.spawn(async { 1 + 2 });
    /// let res = local_ex.wakeup().block_on(local_ex.run(async { task.await * 2 }));
    ///
    /// assert_eq!(res, 6);
    /// ```
    pub async fn run<F>(&self, fut: F) -> F::Output
    where
        F: Future,
    {
        let run_forever = async {
            loop {
                self.tick().await;
            }
        };

        run_forever.or(fut).await
    }
}

impl<'a, W, const C: usize> Default for LocalExecutor<'a, W, C>
where
    W: Wakeup + Default,
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "std")]
mod std {
    use ::std::sync::{Arc, Condvar, Mutex};
    use ::std::task::Wake;

    use crate::*;

    #[cfg(not(any(target_os = "espidf", target_arch = "wasm32")))]
    pub type PlatformExecutor<'a> = LocalExecutor<'a, PlatformWakeup>;
    #[cfg(not(any(target_os = "espidf", target_arch = "wasm32")))]
    pub type PlatformWakeup = StdWakeup;

    /// A `Notification` instance based on `std::sync::Mutex` and `std::sync::Condvar`
    ///
    /// Suitable for STD-compatible platforms where awaking a waker from an ISR
    /// is either not an option (regular operating systems like Linux)
    /// or not necessary.
    pub struct StdWakeup(Mutex<()>, Arc<StdWake>);

    impl StdWakeup {
        pub fn new() -> Self {
            Self(Mutex::new(()), Arc::new(StdWake(Condvar::new())))
        }
    }

    impl Default for StdWakeup {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Wakeup for StdWakeup {
        type Wake = StdWake;

        fn waker(&self) -> Arc<Self::Wake> {
            self.1.clone()
        }
    }

    impl Wait for StdWakeup {
        fn wait(&self) {
            let guard = self.0.lock().unwrap();

            drop(self.1 .0.wait(guard).unwrap());
        }
    }

    pub struct StdWake(Condvar);

    impl Wake for StdWake {
        fn wake(self: Arc<Self>) {
            self.0.notify_one();
        }
    }
}

#[cfg(target_has_atomic = "8")]
mod ceventloop {
    use core::cell::UnsafeCell;
    use core::marker::PhantomData;
    use core::sync::atomic::{AtomicBool, Ordering};

    extern crate alloc;

    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use alloc::task::Wake;

    use crate::*;

    pub type CEventLoopExecutor<'a, S> = LocalExecutor<'a, CEventLoopWakeup<S>>;

    pub type CEventLoopCallback = extern "C" fn(CEventLoopCallbackArg);
    pub type CEventLoopCallbackArg = *mut ();

    /// A generic `Monitor` instance useful for integrating into a native event loop, by scheduling
    /// the execution of the tasks to happen in the event loop.
    ///
    /// Only event loops that provide a way to schedule a piece of "work" in the event loop are
    /// amenable to such integration. Typical event loops include the GLIB event loop, the Matter
    /// C++ SDK event loop, and possibly many others.
    pub struct CEventLoopWakeup<S>(Arc<CEventLoopWake<S>>, PhantomData<*const ()>);

    impl<S> CEventLoopWakeup<S>
    where
        S: FnMut(CEventLoopCallback, CEventLoopCallbackArg),
    {
        pub fn new(scheduler: S) -> Self {
            Self(
                Arc::new(CEventLoopWake {
                    scheduler,
                    scheduled: AtomicBool::new(false),
                    poller: UnsafeCell::new(None),
                }),
                PhantomData,
            )
        }
    }

    impl<S> Wakeup for CEventLoopWakeup<S>
    where
        S: Fn(CEventLoopCallback, CEventLoopCallbackArg) + 'static,
    {
        type Wake = CEventLoopWake<S>;

        fn waker(&self) -> Arc<Self::Wake> {
            self.0.clone()
        }
    }

    impl<S> Schedule for CEventLoopWakeup<S>
    where
        S: Fn(CEventLoopCallback, CEventLoopCallbackArg) + 'static,
    {
        fn schedule_poll_fn<P>(&self, poll_fn: P) -> Result<(), ScheduleError>
        where
            P: FnMut() -> bool + 'static,
        {
            let ctx = &self.0;

            *unsafe { ctx.poller() } = Some(Box::new(poll_fn));

            self.waker().wake();

            Ok(())
        }
    }

    pub struct CEventLoopWake<S> {
        scheduler: S,
        scheduled: AtomicBool,
        poller: UnsafeCell<Option<Box<dyn FnMut() -> bool + 'static>>>,
    }

    impl<S> CEventLoopWake<S> {
        #[allow(clippy::mut_from_ref)]
        unsafe fn poller(&self) -> &mut Option<Box<dyn FnMut() -> bool + 'static>> {
            self.poller.get().as_mut().unwrap()
        }

        extern "C" fn run(arg: CEventLoopCallbackArg) {
            let ctx = unsafe { Arc::from_raw(arg as *const CEventLoopWake<S>) };

            ctx.scheduled.store(false, Ordering::SeqCst);

            if let Some(poll_fn) = unsafe { ctx.poller() } {
                if poll_fn() {
                    *unsafe { ctx.poller() } = None;
                }
            }
        }
    }

    // These are safe, because EventLoopWake cannot be constructed outside of this module, and does not have a public API
    // All calls into the (potentially !Send, !Sync interior of )
    unsafe impl<S> Send for CEventLoopWake<S> {}
    unsafe impl<S> Sync for CEventLoopWake<S> {}

    impl<S> Wake for CEventLoopWake<S>
    where
        S: Fn(CEventLoopCallback, CEventLoopCallbackArg) + 'static,
    {
        fn wake(self: Arc<Self>) {
            if let Ok(false) =
                self.scheduled
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            {
                // TODO: Will leak the Arc if the scheduled event is not executed
                (self.scheduler)(Self::run, Arc::into_raw(self.clone()) as *mut _);
            }
        }
    }
}

#[cfg(all(feature = "std", target_arch = "wasm32"))]
mod wasm {
    use core::marker::PhantomData;

    use ::std::sync::Mutex;

    extern crate alloc;

    use alloc::sync::Arc;

    use js_sys::Promise;

    use wasm_bindgen::prelude::*;

    use crate::*;

    pub type PlatformExecutor<'a> = LocalExecutor<'a, PlatformWakeup>;
    pub type PlatformWakeup = WasmWakeup;

    struct Context {
        promise: Promise,
        closure: Option<Closure<dyn FnMut(JsValue)>>,
    }

    unsafe impl Send for Context {}

    /// A `Wakeup` instance for web-assembly (WASM) based targets.
    ///
    /// Works by integrating the wake instance (and thus the executor) into the WASM event loop.
    ///
    /// Tasks are scheduled for execution in the event loop, by turning those into JavaScript Promises.
    pub struct WasmWakeup(Arc<WasmWake>, PhantomData<*const ()>);

    impl WasmWakeup {
        pub fn new() -> Self {
            Self(
                Arc::new(WasmWake(Mutex::new(Context {
                    promise: Promise::resolve(&JsValue::undefined()),
                    closure: None,
                }))),
                PhantomData,
            )
        }
    }

    impl Default for WasmWakeup {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Wakeup for WasmWakeup {
        type Wake = WasmWake;

        fn waker(&self) -> Arc<Self::Wake> {
            self.0.clone()
        }
    }

    impl Schedule for WasmWakeup {
        fn schedule_poll_fn<P>(&self, mut poll_fn: P) -> Result<(), ScheduleError>
        where
            P: FnMut() -> bool + 'static,
        {
            {
                let ctx = self.0.clone();

                self.0 .0.lock().unwrap().closure = Some(Closure::new(move |_| {
                    if poll_fn() {
                        ctx.0.lock().unwrap().closure = None;
                    }
                }));
            }

            self.waker().wake();

            Ok(())
        }
    }

    pub struct WasmWake(Mutex<Context>);

    impl Wake for WasmWake {
        fn wake(self: Arc<Self>) {
            let ctx = self.0.lock().unwrap();

            if let Some(closure) = ctx.closure.as_ref() {
                let _ = ctx.promise.then(closure);
            }
        }
    }
}

#[cfg(target_os = "espidf")]
mod espidf {
    use esp_idf_hal::task::notification;

    pub use super::*;

    pub type PlatformExecutor<'a> = LocalExecutor<'a, PlatformWakeup>;
    pub type PlatformWakeup = EspWakeup;

    pub type EspWakeup = notification::Notification;
    pub type EspWake = notification::Notifier;

    impl Wakeup for EspWakeup {
        type Wake = EspWake;

        fn waker(&self) -> Arc<Self::Wake> {
            EspWakeup::notifier(self)
        }
    }

    impl Wait for EspWakeup {
        fn wait(&self) {
            self.wait_any()
        }
    }
}
