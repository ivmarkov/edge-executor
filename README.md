# edge-executor

[![CI](https://github.com/ivmarkov/edge-executor/actions/workflows/ci.yml/badge.svg)](https://github.com/ivmarkov/edge-executor/actions/workflows/ci.yml)
[![Documentation](https://docs.rs/edge-executor/badge.svg)](https://docs.rs/edge-executor)

This crate ships a minimal async executor suitable for embedded environments.

`no_std` and ISR-friendly. A thin wrapper around the [async-task](https://github.com/smol-rs/async-task) executor framework, which is also used by [smol](https://github.com/smol-rs/smol).

Suitable for MCU development, bare-metal or on top of an RTOS (e.g. FreeRTOS).

NOTE: This executor DOES require the `alloc` crate and does allocate when spawning a task (and only then).
For a completely static, no-alloc friendly executor, look at [embassy-executor](https://github.com/embassy-rs/embassy/tree/master/embassy-executor).
