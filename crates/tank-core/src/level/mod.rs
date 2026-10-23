//! Level pipeline: raw sensor frames → filtered distance → tank level → state.

pub mod a02yyuw;
pub mod filter;
pub mod state;
pub mod tank;

pub use state::{Event, LevelState, StateMachine};
pub use tank::{Level, Tank};
