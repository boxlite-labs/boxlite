//! BoxLite Core - Shared code for host and guest
//!
//! This crate contains common types, protocols, and utilities
//! used by both the host-side runtime (boxlite) and guest agent.

pub mod constants;
pub mod errors;
pub mod layout;
pub mod transport;

// Generated protobuf types
pub mod generated {
    #![allow(clippy::all, unused_qualifications)]
    tonic::include_proto!("boxlite.v1");
}

pub use errors::{BoxliteError, BoxliteResult};
pub use transport::Transport;

// Container service
pub use generated::container_client::ContainerClient;
pub use generated::container_server::{Container, ContainerServer};

// Guest service
pub use generated::guest_client::GuestClient;
pub use generated::guest_server::{Guest, GuestServer};

// Execution service
pub use generated::execution_client::ExecutionClient;
pub use generated::execution_server::{Execution, ExecutionServer};

// All generated types
pub use generated::*;
