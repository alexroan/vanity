pub mod app;
pub mod backend;
pub mod create2;
pub mod foundry;
pub mod prompts;

#[cfg(feature = "gpu")]
mod gpu;

pub use backend::{
    BackendError, BackendInfo, BackendKind, BackendPreference, BackendSession, SearchEvent,
};
