//! Chronik Stream Kubernetes Operator
//! 
//! This crate provides a Kubernetes operator for managing Chronik Stream clusters.

pub mod controller;
pub mod crd;
pub mod crd_defaults;
pub mod finalizer;
pub mod reconciler;
pub mod resources;

pub use crd::ChronikCluster;