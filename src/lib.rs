//! solo-pool-rs as a library.
//!
//! The binary in `main.rs` is a thin startup sequence over these modules.
//! Exposing them as a library exists for two consumers: the fuzz targets under
//! `fuzz/`, which need to link the network parsers directly, and any future
//! integration test that wants the internals without the process.
pub mod bitcoin;
pub mod config;
pub mod error;
pub mod metrics;
pub mod mining;
pub mod network;
pub mod protocol;
pub mod security;
pub mod settings;
pub mod stats;
