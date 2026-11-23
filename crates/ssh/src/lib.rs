//! Machines reached by SSH: a file or a folder on one as a place to read
//! from.
//!
//! The connection is the system's own `ssh`, asked for the SFTP subsystem.
//! So every host, key, agent, jump host and option that `ssh` knows works
//! here, and the other machine needs nothing but its SSH server.
//!
//! This crate knows how to ask for what a folder holds and for bytes of a
//! file. It does no scheduling, retrying or checking of its own: a
//! [`Remote`] is a [`model::place::Source`], and the engine does the rest.

// Every public item is documented.
#![warn(missing_docs)]
// A library must not panic on bad input. Tests may.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

mod remote;
mod session;
#[cfg(test)]
mod tests;
pub mod wire;

pub use remote::{Options, Remote};

use model::location::Protocol;

/// The kinds of place this crate provides.
pub const PROTOCOLS: [Protocol; 1] = [Protocol {
    scheme: "ssh",
    reads: true,
    writes: false,
    about: "a file or a folder on a machine reached by SSH, written `host:path`",
}];
