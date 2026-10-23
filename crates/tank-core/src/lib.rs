//! Hardware-independent core of the tank level monitor.
//!
//! Everything in this crate is `no_std` and free of esp-hal types so it can be
//! unit-tested on the host (`cargo test -p tank-core --target <host>`) and
//! reused if the MCU changes. Hardware wiring lives in the firmware crate at
//! the repository root (`src/main.rs` and `src/app/`).
#![no_std]

pub mod config;
pub mod level;
pub mod payload;

/// Firmware version reported to the server and shown in the UIs.
pub const FIRMWARE_VERSION: &str = env!("CARGO_PKG_VERSION");
