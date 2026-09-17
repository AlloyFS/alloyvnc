//! The server: one capture thread writing a shared framebuffer, one session
//! per client reading it, updates sent when a client asks and something has
//! changed.
//!
//! The library half exists so the integration tests and the test client can
//! drive a server in-process on an ephemeral port.

pub mod capture;
pub mod client;
pub mod server;
pub mod session;
pub mod shared;
