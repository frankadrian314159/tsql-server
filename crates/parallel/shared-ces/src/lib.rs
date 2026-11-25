//! Types shared by every node of the distributed tsql-server (control, execute, storage).
//!
//! * [`config`]: the cluster's static topology;
//! * [`wire`]: the messages of the control plane (carried as Arrow Flight actions and tickets, encoded with postcard);
//! * [`arrowconv`]: a result set of the engine as Arrow record batches and back (the data plane);
//! * [`metrics`]: CPU, memory, disk and network samples of a node, and their Prometheus export;
//! * [`route`]: the stable hash that decides which shard owns a key;
//! * [`tls`]: mutual TLS between the nodes, and the certificates it uses.
pub mod arrowconv;
pub mod config;
pub mod metrics;
pub mod route;
pub mod tls;
pub mod wire;

pub use config::{ClusterConfig, ShardInfo};
pub use wire::{Fragment, RunReply, WireError, WireEvent, Work};
