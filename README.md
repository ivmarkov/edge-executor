# edge-executor

This crate ships an async executor suitable for embedded environments.

`no_std` and ISR-friendly. A thin wrapper around the [async-task](https://github.com/smol-rs/async-task) executor framework, which is also used by [smol](https://github.com/smol-rs/smol).

Suitable for MCU development, bare-metal or on top of an RTOS (e.g. FreeRTOS).
