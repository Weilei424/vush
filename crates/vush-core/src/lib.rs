//! Core engine for Vush.
//!
//! This crate owns the immutable `WorkloadPlan` (see [`plan`]) and the
//! protocol boundary (see [`protocol`]) and the request driver (see
//! [`runner`]), and will own the workload coordinator and scheduler, the VU
//! manager, and the rest of the scenario runner. It
//! must stay protocol-agnostic: a protocol adapter such as `vush-http`
//! implements the boundary's executor trait, and this crate never depends
//! on one.

pub mod plan;
pub mod protocol;
pub mod runner;
