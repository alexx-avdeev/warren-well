//! Hardware-facing application tasks. Everything that touches esp-hal,
//! esp-radio, the network stack or flash lives here.

pub mod ble;
pub mod control;
pub mod level_task;
pub mod reporter;
pub mod state;
pub mod storage;
pub mod web;
pub mod wifi;

/// `static` initialisation helper (stable-Rust alternative to `StaticCell::make_static`).
#[macro_export]
macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write($val);
        x
    }};
}
