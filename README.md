# edge-executor

[![CI](https://github.com/ivmarkov/edge-executor/actions/workflows/ci.yml/badge.svg)](https://github.com/ivmarkov/edge-executor/actions/workflows/ci.yml)
![crates.io](https://img.shields.io/crates/v/edge-executor.svg)
[![Documentation](https://docs.rs/edge-executor/badge.svg)](https://docs.rs/edge-executor)

This crate ships a minimal async executor suitable for microcontrollers and embedded systems in general.

A `no_std` drop-in replacement for [smol](https://github.com/smol-rs/smol)'s [async-executor](https://github.com/smol-rs/async-executor), with the implementation being a thin wrapper around [smol](https://github.com/smol-rs/smol)'s [async-task](https://github.com/smol-rs/async-task) as well.

```rust
// ESP-IDF example, local execution, local borrows.
// With STD enabled, you can also just use `edge_executor::block_on` 
// instead of `esp_idf_svc::hal::task::block_on`.

use edge_executor::LocalExecutor;
use esp_idf_svc::hal::task::block_on;

fn main() {
    let local_ex: LocalExecutor = Default::default();

    // Borrowed by `&mut` inside the future spawned on the executor
    let mut data = 3;

    let data = &mut data;

    let task = local_ex.spawn(async move {
        *data += 1;

        *data
    });

    let res = block_on(local_ex.run(async { task.await * 2 }));

    assert_eq!(res, 8);
}
```

```rust
// STD example, work-stealing execution.

use async_channel::unbounded;
use easy_parallel::Parallel;

use edge_executor::{Executor, block_on};

fn main() {
    let ex: Executor = Default::default();
    let (signal, shutdown) = unbounded::<()>();

    Parallel::new()
        // Run four executor threads.
        .each(0..4, |_| block_on(ex.run(shutdown.recv())))
        // Run the main future on the current thread.
        .finish(|| block_on(async {
            println!("Hello world!");
            drop(signal);
        }));
}
```

```rust
// WASM example.

use log::{info, Level};

use edge_executor::LocalExecutor;

use static_cell::StaticCell;
use wasm_bindgen_futures::spawn_local;

use gloo_timers::future::TimeoutFuture;

static LOCAL_EX: StaticCell<LocalExecutor> = StaticCell::new();

fn main() {
    console_log::init_with_level(Level::Info).unwrap();

    // Local executor (futures can be `!Send`) yet `'static`
    let local_ex = &*LOCAL_EX.init(Default::default());

    local_ex
        .spawn(async {
            loop {
                info!("Tick");
                TimeoutFuture::new(1000).await;
            }
        })
        .detach();

    spawn_local(local_ex.run(core::future::pending::<()>()));
}
```

**Highlights**

- `no_std` (but does need `alloc`):
  - The executor uses allocations in a controlled way: only when a new task is being spawn, as well as during the construction of the executor itself;
  - For a `no_std` *and* "no_alloc" executor, look at [embassy-executor](https://github.com/embassy-rs/embassy/tree/main/embassy-executor), which statically pre-allocates all tasks.

- Works on targets which have no `core::sync::atomic` support, thanks to [portable-atomic](https://github.com/taiki-e/portable-atomic);
  - NOTE: When enabling the `portable-atomic` feature, please patch the `async-task` dependency to point to its GH repo, because the `portable-atomic` support in `async-task` is not released on crates.io yet.

- Does not assume an RTOS and can run completely bare-metal too;

- Lockless, atomic-based, bounded task queue by default, which works well for waking the executor directly from an ISR on e.g. FreeRTOS or ESP-IDF (unbounded also an option with feature `unbounded`, yet that might mean potential allocations in an ISR context, which should be avoided).

**Great features carried over from [async-executor](https://github.com/smol-rs/async-executor)**:

- Stack borrows: futures spawned on the executor need to live only as long as the executor itself. No `F: Future + 'static` constraints;

- Completely portable and async. `Executor::run` simply returns a `Future`. Polling this future runs the executor, i.e. `block_on(executor.run(core::future:pending::<()>()))`;

- `const new` constructor function.
