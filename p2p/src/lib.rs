//! Nakamoto's peer-to-peer library.
//!
//! The `p2p` crate implements the core protocol state-machine. It can be found under the
//! [protocol](crate::protocol) module, which has the following sub-protocol:
//!
//! * [`AddressManager`][addrmgr]: handles peer address exchange
//! * [`SyncManager`][syncmgr]: handles block header sync
//! * [`ConnectionManager`][connmgr]: handles peer connections
//! * [`PingManager`][pingmgr]: handles pings and pongs
//! * [`SpvManager`][spvmgr]: handles compact filter sync
//! * [`PeerManager`][peermgr]: handles peer handshake
//!
//! [addrmgr]: crate::protocol::addrmgr::AddressManager
//! [syncmgr]: crate::protocol::syncmgr::SyncManager
//! [connmgr]: crate::protocol::connmgr::ConnectionManager
//! [pingmgr]: crate::protocol::pingmgr::PingManager
//! [spvmgr]: crate::protocol::spvmgr::SpvManager
//! [peermgr]: crate::protocol::peermgr::PeerManager
//!
//! Nakamoto's implementation of the peer-to-peer protocol(s) is *I/O-free*. The
//! core logic is implemented as a state machine with *inputs* and *outputs* and a
//! *step* function that does not perform any network I/O.
//!
//! The reason for this is to keep the protocol code easy to read and simple to
//! test. Not having I/O minimizes the possible error states and error-handling
//! code in the protocol, and allows for a fully *deterministic* protocol. This
//! means failing tests can always be reproduced and 100% test coverage is within
//! reach.
//!
//! To achieve this, handling of network I/O is cleanly separated into a network
//! *reactor*. See the `nakamoto-net-poll` crate for an example of a reactor.
//!
#![allow(clippy::type_complexity)]
#![allow(clippy::new_without_default)]
#![allow(clippy::single_match)]
#![allow(clippy::comparison_chain)]
#![allow(clippy::inconsistent_struct_constructor)]
#![allow(clippy::too_many_arguments)]
#![deny(missing_docs, unsafe_code)]
pub mod error;
pub mod event;
pub mod protocol;
pub mod reactor;
pub use bitcoin;

pub use protocol::{Link, PeerId};
