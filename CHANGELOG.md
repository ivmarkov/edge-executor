# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2023-10-17
* Crate goals clarified
* Crate redesigned to follow the API of `async-executor`
* Re-implemented as a completely portable executor which is itself a `Future` (platform/OS is expected to provide a `block_on` or "spawn a `Future` onto an event loop" primitive, but the executor is unaware of it)
* Documentation, examples, tests
* More user-friendly README
