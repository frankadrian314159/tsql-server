//! Types shared by every node of the distributed tsql-server (control, execute, storage).
//!
//! * [`config`]: the cluster's static topology;
//! * [`wire`]: the messages of the control plane (carried as Arrow Flight actions and tickets, encoded with postcard);
//! * [`arrowconv`]: a result set of the engine as Arrow record batches and back (the data plane);
//! * [`route`]: the stable hash that decides which shard owns a key.
pub mod arrowconv;
pub mod config;
pub mod route;
pub mod wire;

pub use config::{ClusterConfig, ShardInfo};
pub use wire::{Fragment, RunReply, WireError, WireEvent, Work};
