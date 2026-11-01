//! Core engine for Vush.
//!
//! This crate will own the immutable `WorkloadPlan`, the workload
//! coordinator and scheduler, the VU manager, and the scenario runner.
//! It must stay protocol-agnostic: protocol adapters plug in through a
//! protocol-neutral executor interface, and this crate never depends on a
//! specific protocol implementation such as `vush-http`.
