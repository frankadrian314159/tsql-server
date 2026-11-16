//! Operating a cluster: resource metrics of the control node and the shards, their Prometheus export, waiting for the shards
//! at start-up, and the control node's part of a graceful stop.
use crate::Cluster;
use futures::future::join_all;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tsql_server::SqlError;
use tsql_shared_ces::metrics::{render_prometheus, NodeMetrics, Sampler};

/// The sampler of this process (one is enough: it reads the machine).
fn sampler() -> &'static Sampler {
    static S: OnceLock<Sampler> = OnceLock::new();
    S.get_or_init(|| Sampler::start(Duration::from_secs(1)))
}

fn started() -> Instant {
    static T: OnceLock<Instant> = OnceLock::new();
    *T.get_or_init(Instant::now)
}

impl Cluster {
    /// The resource use of the machine this control node runs on.
    pub fn local_metrics(&self) -> NodeMetrics {
        NodeMetrics { sample: sampler().sample(), uptime_secs: started().elapsed().as_secs(), ..Default::default() }
    }

    /// What each shard reports: CPU, memory, disk and network use and the engine's counters (an unreachable shard gives its
    /// error). Must not be called from a task of the cluster's own runtime.
    pub fn metrics(self: &Arc<Self>) -> Vec<Result<NodeMetrics, SqlError>> {
        let shards = self.shard_list();
        self.rt.block_on(async { join_all(shards.iter().map(|s| s.metrics())).await })
    }

    /// The metrics of the control node and the shards in the Prometheus text format, with whether each shard answers and how
    /// many hash slots it owns: what `tsql-cluster --metrics-listen` serves.
    pub fn prometheus(self: &Arc<Self>) -> String {
        let mut nodes = vec![("control".to_string(), self.local_metrics())];
        let (mut up, slots) = (String::new(), self.slot_counts());
        for (i, m) in self.metrics().into_iter().enumerate() {
            let label = format!("shard-{}", i);
            up.push_str(&format!("tsql_shard_up{{node=\"{}\"}} {}\ntsql_slots{{node=\"{}\"}} {}\n", label, if m.is_ok() { 1 } else { 0 }, label, slots.get(i).copied().unwrap_or(0)));
            if let Ok(m) = m {
                nodes.push((label, m));
            }
        }
        format!("{}# HELP tsql_shard_up 1 when the shard answers\n# TYPE tsql_shard_up gauge\n# HELP tsql_slots hash slots the shard owns\n# TYPE tsql_slots gauge\n{}", render_prometheus(&nodes), up)
    }

    /// Wait until every shard answers (they may be starting at the same time as this control node). Err: the shards that did
    /// not, after `timeout`.
    pub fn wait_for_shards(self: &Arc<Self>, timeout: Duration) -> Result<(), Vec<usize>> {
        let deadline = Instant::now() + timeout;
        loop {
            let shards = self.shard_list();
            let down: Vec<usize> = self.rt.block_on(async { join_all(shards.iter().map(|s| s.status())).await }).into_iter().enumerate().filter(|(_, r)| r.is_err()).map(|(i, _)| i).collect();
            if down.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(down);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// The control node's part of a graceful stop, after its listeners have stopped taking sessions: transactions that were
    /// decided but not confirmed everywhere stay in the state file (they are settled at the next start), and the state is
    /// written.
    pub fn shutdown(self: &Arc<Self>) {
        self.save_state();
    }
}
