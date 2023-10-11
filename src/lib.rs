#![cfg_attr(not(feature = "std"), no_std)]

// #[cfg(feature = "portable-atomic")]
// compile_error!(
//     "Support for targets without atomics is incomplete as `async-task` does not yet support those."
// );

#[cfg(all(feature = "heapless", feature = "unbounded"))]
compile_error!("Feature `heapless` is not compatible with feature `unbounded`.");

use core::cell::OnceCell;
use core::future::{poll_fn, Future};
use core::marker::PhantomData;
use core::task::{Context, Poll};

extern crate alloc;

use alloc::rc::Rc;

use async_task::Runnable;

pub use async_task::{FallibleTask, Task};

use atomic_waker::AtomicWaker;
use futures_lite::FutureExt;

#[cfg(not(feature = "portable-atomic"))]
use alloc::sync::Arc;
#[cfg(feature = "portable-atomic")]
use portable_atomic_util::Arc;

#[cfg(feature = "std")]
pub use futures_lite::future::block_on;

/// An async executor.
///
/// # Examples
///
/// A multi-threaded executor:
///
/// ```ignore
/// use async_channel::unbounded;
/// use easy_parallel::Parallel;
///
/// use edge_executor::{Executor, block_on};
///
/// let ex: Executor = Default::default();
/// let (signal, shutdown) = unbounded::<()>();
///
/// Parallel::new()
///     // Run four executor threads.
///     .each(0..4, |_| block_on(ex.run(shutdown.recv())))
///     // Run the main future on the current thread.
///     .finish(|| block_on(async {
///         println!("Hello world!");
///         drop(signal);
///     }));
/// ```
pub struct Executor<'a, const C: usize = 64> {
    state: OnceCell<State<C>>,
    _invariant: PhantomData<core::cell::UnsafeCell<&'a ()>>,
}

impl<'a, const C: usize> Executor<'a, C> {
    /// Creates a new executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use edge_executor::Executor;
    ///
    /// let ex: Executor = Default::default();
    /// ```
    pub const fn new() -> Self {
        Self {
            state: OnceCell::new(),
            _invariant: PhantomData,
        }
    }

    /// Spawns a task onto the executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use edge_executor::Executor;
    ///
    /// let ex: Executor = Default::default();
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// ```
    ///
    /// Note that if the executor's queue size is equal to the number of currently
    /// spawned and running tasks, spawning this additional task might cause the executor to panic
    /// later, when the task is scheduled for polling.
    pub fn spawn<F>(&self, fut: F) -> Task<F::Output>
    where
        F: Future + Send + 'a,
        F::Output: Send + 'a,
    {
        unsafe { self.spawn_unchecked(fut) }
    }

    /// Attempts to run a task if at least one is scheduled.
    ///
    /// Running a scheduled task means simply polling its future once.
    ///
    /// # Examples
    ///
    /// ```
    /// use edge_executor::Executor;
    ///
    /// let ex: Executor = Default::default();
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
    /// use edge_executor::{Executor, block_on};
    ///
    /// let ex: Executor = Default::default();
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// block_on(ex.tick()); // runs the task
    /// ```
    pub async fn tick(&self) {
        self.runnable().await.run();
    }

    /// Runs the executor asynchronously until the given future completes.
    ///
    /// # Examples
    ///
    /// ```
    /// use edge_executor::{Executor, block_on};
    ///
    /// let ex: Executor = Default::default();
    ///
    /// let task = ex.spawn(async { 1 + 2 });
    /// let res = block_on(ex.run(async { task.await * 2 }));
    ///
    /// assert_eq!(res, 6);
    /// ```
    pub async fn run<F>(&self, fut: F) -> F::Output
    where
        F: Future + Send + 'a,
    {
        unsafe { self.run_unchecked(fut).await }
    }

    /// Waits for the next runnable task to run.
    async fn runnable(&self) -> Runnable {
        poll_fn(|ctx| self.poll_runnable(ctx)).await
    }

    /// Polls the first task scheduled for execution by the executor.
    fn poll_runnable(&self, ctx: &Context<'_>) -> Poll<Runnable> {
        self.state().waker.register(ctx.waker());

        if let Some(runnable) = self.try_runnable() {
            Poll::Ready(runnable)
        } else {
            Poll::Pending
        }
    }

    /// Pops the first task scheduled for execution by the executor.
    ///
    /// Returns
    /// - `None` - if no task was scheduled for execution
    /// - `Some(Runnnable)` - the first task scheduled for execution. Calling `Runnable::run` will
    ///    execute the task. In other words, it will poll its future.
    fn try_runnable(&self) -> Option<Runnable> {
        let runnable;

        #[cfg(not(feature = "heapless"))]
        {
            runnable = self.state().queue.pop();
        }

        #[cfg(feature = "heapless")]
        {
            runnable = self.state().queue.dequeue();
        }

        runnable
    }

    unsafe fn spawn_unchecked<F>(&self, fut: F) -> Task<F::Output>
    where
        F: Future,
    {
        let schedule = {
            let queue = self.state().queue.clone();
            let waker = self.state().waker.clone();

            move |runnable| {
                #[cfg(all(not(feature = "heapless"), feature = "unbounded"))]
                {
                    queue.push(runnable);
                }

                #[cfg(all(not(feature = "heapless"), not(feature = "unbounded")))]
                {
                    queue.push(runnable).unwrap();
                }

                #[cfg(feature = "heapless")]
                {
                    queue.enqueue(runnable).unwrap();
                }

                if let Some(waker) = waker.take() {
                    waker.wake();
                }
            }
        };

        let (runnable, task) = unsafe { async_task::spawn_unchecked(fut, schedule) };

        runnable.schedule();

        task
    }

    async unsafe fn run_unchecked<F>(&self, fut: F) -> F::Output
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

    /// Returns a reference to the inner state.
    fn state(&self) -> &State<C> {
        self.state.get_or_init(State::new)
    }
}

impl<'a, const C: usize> Default for Executor<'a, C> {
    fn default() -> Self {
        Self::new()
    }
}

unsafe impl<'a, const C: usize> Send for Executor<'a, C> {}
unsafe impl<'a, const C: usize> Sync for Executor<'a, C> {}

/// A thread-local executor.
///
/// The executor can only be run on the thread that created it.
///
/// # Examples
///
/// ```
/// use edge_executor::{LocalExecutor, block_on};
///
/// let local_ex: LocalExecutor = Default::default();
///
/// block_on(local_ex.run(async {
///     println!("Hello world!");
/// }));
/// ```
pub struct LocalExecutor<'a, const C: usize = 64> {
    executor: Executor<'a, C>,
    _not_send: PhantomData<core::cell::UnsafeCell<&'a Rc<()>>>,
}

#[allow(clippy::missing_safety_doc)]
impl<'a, const C: usize> LocalExecutor<'a, C> {
    /// Creates a single-threaded executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use edge_executor::LocalExecutor;
    ///
    /// let local_ex: LocalExecutor = Default::default();
    /// ```
    pub const fn new() -> Self {
        Self {
            executor: Executor::<C>::new(),
            _not_send: PhantomData,
        }
    }

    /// Spawns a task onto the executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use edge_executor::LocalExecutor;
    ///
    /// let local_ex: LocalExecutor = Default::default();
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
        unsafe { self.executor.spawn_unchecked(fut) }
    }

    /// Attempts to run a task if at least one is scheduled.
    ///
    /// Running a scheduled task means simply polling its future once.
    ///
    /// # Examples
    ///
    /// ```
    /// use edge_executor::LocalExecutor;
    ///
    /// let local_ex: LocalExecutor = Default::default();
    /// assert!(!local_ex.try_tick()); // no tasks to run
    ///
    /// let task = local_ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// assert!(local_ex.try_tick()); // a task was found
    /// ```    
    pub fn try_tick(&self) -> bool {
        self.executor.try_tick()
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
    /// use edge_executor::{LocalExecutor, block_on};
    ///
    /// let local_ex: LocalExecutor = Default::default();
    ///
    /// let task = local_ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// block_on(local_ex.tick()); // runs the task
    /// ```
    pub async fn tick(&self) {
        self.executor.tick().await
    }

    /// Runs the executor asynchronously until the given future completes.
    ///
    /// # Examples
    ///
    /// ```
    /// use edge_executor::{LocalExecutor, block_on};
    ///
    /// let local_ex: LocalExecutor = Default::default();
    ///
    /// let task = local_ex.spawn(async { 1 + 2 });
    /// let res = block_on(local_ex.run(async { task.await * 2 }));
    ///
    /// assert_eq!(res, 6);
    /// ```
    pub async fn run<F>(&self, fut: F) -> F::Output
    where
        F: Future,
    {
        unsafe { self.executor.run_unchecked(fut) }.await
    }
}

impl<'a, const C: usize> Default for LocalExecutor<'a, C> {
    fn default() -> Self {
        Self::new()
    }
}

struct State<const C: usize> {
    #[cfg(all(not(feature = "heapless"), feature = "unbounded"))]
    queue: Arc<crossbeam_queue::SegQueue<Runnable>>,
    #[cfg(all(not(feature = "heapless"), not(feature = "unbounded")))]
    queue: Arc<crossbeam_queue::ArrayQueue<Runnable>>,
    #[cfg(feature = "heapless")]
    queue: Arc<heapless::mpmc::MpMcQueue<Runnable, C>>,
    waker: Arc<AtomicWaker>,
}

impl<const C: usize> State<C> {
    fn new() -> Self {
        Self {
            #[cfg(all(not(feature = "heapless"), feature = "unbounded"))]
            queue: Arc::new(crossbeam_queue::SegQueue::new()),
            #[cfg(all(not(feature = "heapless"), not(feature = "unbounded")))]
            queue: Arc::new(crossbeam_queue::ArrayQueue::new(C)),
            #[cfg(feature = "heapless")]
            queue: Arc::new(heapless::mpmc::MpMcQueue::new()),
            waker: Arc::new(AtomicWaker::new()),
        }
    }
}
