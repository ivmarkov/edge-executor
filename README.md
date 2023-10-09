# edge-executor

[![CI](https://github.com/ivmarkov/edge-executor/actions/workflows/ci.yml/badge.svg)](https://github.com/ivmarkov/edge-executor/actions/workflows/ci.yml)
![crates.io](https://img.shields.io/crates/v/edge-executor.svg)
[![Documentation](https://docs.rs/edge-executor/badge.svg)](https://docs.rs/edge-executor)

This crate ships a minimal async executor suitable for microcontrollers.

The implementation is a thin wrapper around [smol](::smol)'s [async-task](::async-task) crate.

**Highlights**

- `no_std` (but does need `alloc`; for a `no_std` *and* "no_alloc" executor, look at [Embassy](::embassy), which statically pre-allocates all tasks);
           (note also that the executor uses allocations in a limited way: when a new task is being spawn, as well as the executor itself);

- Follow closely the API of [smol](::smol)'s [async-executor](::async-executor) crate, so that it can serve as a (mostly) drop-in replacement;

- Does not assume an RTOS and can run completely bare-metal (or on top of an RTOS);

- Local execution only. No plans for implementing work-stealing execution, as threads are either a scarce resource on microcontrollers' RTOS,
  or do not exist at all (Rust bare-metal);

- Pluggable `Wakeup` mechanism which makes it customizable for different microcontrollers;

- ISR-friendly, i.e. tasks can be woken up (and thus re-scheduled) from within an ISR
  (feature `wake-from-isr` should be enabled);

- `StdWakeup` implementation based on a mutex + condvar pair, usable on top of the Rust Standard Library;

- `EspWakeup` implementation for ESP-IDF based on FreeRTOS task notifications, compatible with the `wake-from-isr` feature;

- `WasmWakeup` implementation for the WASM event loop, compatible with WASM;

- `EventLoopWakeup` implementation for native event loops like those of GLIB, the Matter C++ SDK and others.
