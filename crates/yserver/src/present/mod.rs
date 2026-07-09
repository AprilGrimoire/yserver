pub mod event_loop;
pub mod paint;
pub mod state;

pub use paint::paint;
pub use state::{State, update};
