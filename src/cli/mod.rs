//! Command dispatch for the imperative `testsprite-rs <group> <cmd>` CLI.
//!
//! The clap tree lives in `main.rs`; the larger per-group dispatchers live here
//! so the binary root stays a thin parse-and-route layer. Today only the `test`
//! group (by far the largest surface) is split out; the other `run_*` handlers
//! can move here the same way as they grow.

pub mod test;
