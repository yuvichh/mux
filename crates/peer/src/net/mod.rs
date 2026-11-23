//! Networking: node identity, authenticated QUIC connections and the wire
//! protocol. This layer knows nothing about disks or swarms.

mod endpoint;
mod identity;
pub mod proto;

pub use endpoint::{Congestion, Connection, Endpoint, Incoming, Options, Relays};
pub(crate) use identity::random_secret;
pub use identity::{Identity, random_code};
pub use model::ticket::NodeId;
pub use proto::PeerInfo;
