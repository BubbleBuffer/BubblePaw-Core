//! Encrypted transport primitives for hosts and paired devices.
#![forbid(unsafe_code)]

pub mod device;
pub mod frame;
pub mod pairing;
pub mod relay;
pub mod store;
pub mod trust;
pub mod tunnel;

pub use error::{Error, Result};
mod error;
