//! spar: two coding agents alternate implementing and reviewing GitHub issues
//! until a pull request converges.
//!
//! A single model reviewing its own work grades its own homework. Two models
//! with different training and different failure modes catch different things,
//! and the disagreements are the useful part.

pub mod agent;
pub mod checkin;
pub mod cli;
pub mod comments;
pub mod config;
pub mod error;
pub mod followups;
pub mod jsonx;
pub mod logging;
pub mod model;
pub mod proc;
pub mod repo;
pub mod review;
pub mod review_only;
pub mod schema;
pub mod style;
pub mod textsim;
pub mod tracker;
pub mod triage;

pub use error::{Result, SparError};
