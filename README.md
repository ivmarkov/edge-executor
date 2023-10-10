# edge-executor

[![CI](https://github.com/ivmarkov/edge-executor/actions/workflows/ci.yml/badge.svg)](https://github.com/ivmarkov/edge-executor/actions/workflows/ci.yml)
![crates.io](https://img.shields.io/crates/v/edge-executor.svg)
[![Documentation](https://docs.rs/edge-executor/badge.svg)](https://docs.rs/edge-executor)

This crate ships a minimal async executor suitable for microcontrollers and embedded systems in general.

A `no_std` drop-in replacement for [smol](https://github.com/smol-rs/smol)'s [async-executor](https://github.com/smol-rs/async-executor), with the implementation being a thin wrapper around [smol](https://github.com/smol-rs/smol)'s [async-task](https://github.com/smol-rs/async-task) as well.

**Highlights**

- `no_std` (but does need `alloc`):
  - The executor uses allocations in a controlled way: only when a new task is being spawn, as well as during the construction of the executor itself;
  - For a `no_std` *and* "no_alloc" executor, look at [embassy-executor](https://github.com/embassy-rs/embassy/tree/main/embassy-executor), which statically pre-allocates all tasks.

- Works on targets which have no `core::sync::atomic` support, thanks to [portable-atomic](https://github.com/taiki-e/portable-atomic);

- Does not assume an RTOS and can run completely bare-metal too;

- Lockless, atomic-based, bounded task queue by default, which works well for waking the executor directly from an ISR on e.g. FreeRTOS or ESP-IDF (unbounded also an option with feature `unbounded`, yet that might mean potential allocations in an ISR context, which should be avoided).

**Great features carried over from [async-executor](https://github.com/smol-rs/async-executor)**:

- Stack borrows: futures spawned on the executor need to live only as long as the executor itself. No `F: Future + 'static` constraints;

- Completely portable and async. `Executor::run` simply returns a `Future`. Polling this future runs the executor, i.e. `block_on(executor.run(core::task:forever::<()>()))`;

**TODO**:

Upstream the `portable-atomic` dependency into `async-task` (and possibly into `crossbeam-queue`) so that the crate can compile on targets that do not support `core::sync` atomics.
