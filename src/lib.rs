//! spar: two coding agents alternate implementing and reviewing GitHub issues
//! until a pull request converges.
//!
//! A single model reviewing its own work grades its own homework. Two models
//! with different training and different failure modes catch different things,
//! and the disagreements are the useful part.

pub mod agent;
pub mod cli;
pub mod config;
pub mod error;
pub mod jsonx;
pub mod logging;
pub mod model;
pub mod proc;
pub mod repo;
pub mod review;
pub mod schema;
pub mod style;
pub mod triage;

pub use error::{Result, SparError};
