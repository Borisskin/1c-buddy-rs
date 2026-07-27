//! HTTPS client for the 1C assistant service.

mod client;
mod compat;
mod sse;
mod tool_roundtrip;
mod types;

pub(crate) use client::{CallContext, NaparnikClient};
