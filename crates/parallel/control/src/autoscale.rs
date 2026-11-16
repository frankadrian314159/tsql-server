//! Autoscaling: watching the shards' resource use and growing or shrinking the cluster when it crosses the thresholds of a
//! [`ScalePolicy`](crate::rebalance::ScalePolicy).
//!
//! The cluster cannot make machines, so a [`Provisioner`] does: `add_node` brings up a `tsql-shard` and returns its address,
//! `remove_node` takes one away. [`CommandProvisioner`] runs shell commands for that (a `gcloud compute instances create ...`
//! that prints the new node's `host:port`, and one that deletes it), which is how `tsql-cluster --autoscale` works.
//!
//! An [`Autoscaler`] takes a reading (`step`) every interval and acts only on a *sustained* change: a growth advice has to be
//! given by each of the last `sustain` readings (so a burst of load does not add a machine), and after any change it waits
//! `cooldown_secs` (moving data loads the machines, which must not read as demand). Growing adds the shard and moves slots to it;
//! shrinking drains the last shard (the only one that can be removed), then lets the provisioner delete the machine.
use crate::rebalance::{advise, ScaleAdvice, ScalePolicy, ShardTelemetry};
use crate::Cluster;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Brings shard nodes up and takes them away.
pub trait Provisioner: Send {
    /// Start a shard node (empty, ready to serve) and return its address, `host:port`.
    fn add_node(&mut self) -> Result<String, String>;
    /// Stop (and delete) the shard node at this address; its data has been moved to the other shards.
    fn remove_node(&mut self, addr: &str) -> Result<(), String>;
}

/// A provisioner that runs shell commands. `up` must print the new node's address on its last line of standard output and exit
/// when the node is ready; `down` gets the address in the environment variable `TSQL_SHARD_ADDR`.
pub struct CommandProvisioner {
    pub up: String,
    pub down: String,
}

fn shell(cmd: &str) -> std::process::Command {
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    }
}

impl Provisioner for CommandProvisioner {
    fn add_node(&mut self) -> Result<String, String> {
        let out = shell(&self.up).output().map_err(|e| format!("cannot run {:?}: {}", self.up, e))?;
        if !out.status.success() {
            return Err(format!("{:?} failed: {}", self.up, String::from_utf8_lossy(&out.stderr).trim()));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let addr = text.lines().rev().map(str::trim).find(|l| !l.is_empty()).ok_or_else(|| format!("{:?} printed no address", self.up))?;
        Ok(addr.to_string())
    }

    fn remove_node(&mut self, addr: &str) -> Result<(), String> {
        let out = shell(&self.down).env("TSQL_SHARD_ADDR", addr).output().map_err(|e| format!("cannot run {:?}: {}", self.down, e))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!("{:?} failed: {}", self.down, String::from_utf8_lossy(&out.stderr).trim()))
        }
    }
}

/// What a step did.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// nothing to do (the reason, for the log)
    Nothing(String),
    Added { shard: usize, addr: String, reason: String },
    Removed { addr: String, reason: String },
    Rebalanced { moved: usize, reason: String },
    /// a change was due but could not be made
    Failed(String),
}

pub struct Autoscaler {
    cluster: Arc<Cluster>,
    pub policy: ScalePolicy,
    provisioner: Box<dyn Provisioner>,
    history: VecDeque<Vec<ShardTelemetry>>,
    last_change: Option<Instant>,
}

/// The advice that every one of the last `window` readings gave (as the growth or shrink it was), if the window is full: what
/// is *sustained*. Rebalancing and unreachable shards are not sustained things (the latest reading decides).
pub fn sustained(history: &VecDeque<Vec<ShardTelemetry>>, policy: &ScalePolicy) -> Vec<ScaleAdvice> {
    let window = policy.sustain.max(1);
    let latest = match history.back() {
        Some(t) => advise(t, policy),
        None => return vec![],
    };
    let mut out: Vec<ScaleAdvice> = latest.iter().filter(|a| matches!(a, ScaleAdvice::Rebalance { .. } | ScaleAdvice::Unreachable { .. } | ScaleAdvice::Bottleneck { .. })).cloned().collect();
    if history.len() < window {
        return out;
    }
    let recent: Vec<Vec<ScaleAdvice>> = history.iter().rev().take(window).map(|t| advise(t, policy)).collect();
    let all = |f: &dyn Fn(&ScaleAdvice) -> bool| recent.iter().all(|a| a.iter().any(|x| f(x)));
    if all(&|a| matches!(a, ScaleAdvice::AddShard { .. })) {
        out.extend(latest.iter().filter(|a| matches!(a, ScaleAdvice::AddShard { .. })).cloned());
    } else if all(&|a| matches!(a, ScaleAdvice::RemoveShard { .. })) {
        out.extend(latest.iter().filter(|a| matches!(a, ScaleAdvice::RemoveShard { .. })).cloned());
    }
    out
}

impl Autoscaler {
    pub fn new(cluster: Arc<Cluster>, policy: ScalePolicy, provisioner: Box<dyn Provisioner>) -> Autoscaler {
        Autoscaler { cluster, policy, provisioner, history: VecDeque::new(), last_change: None }
    }

    /// Use another policy (the readings so far were judged by the old one: they are forgotten).
    pub fn set_policy(&mut self, policy: ScalePolicy) {
        self.policy = policy;
        self.history.clear();
    }

    /// Add a reading (for tests and for callers that have their own source) and decide.
    pub fn observe(&mut self, telemetry: Vec<ShardTelemetry>) -> Action {
        self.history.push_back(telemetry);
        while self.history.len() > self.policy.sustain.max(1) + 1 {
            self.history.pop_front();
        }
        if let Some(t) = self.last_change {
            if t.elapsed() < Duration::from_secs(self.policy.cooldown_secs) {
                return Action::Nothing(format!("cooling down ({}s of {}s)", t.elapsed().as_secs(), self.policy.cooldown_secs));
            }
        }
        let advice = sustained(&self.history, &self.policy);
        for a in &advice {
            match a {
                ScaleAdvice::Unreachable { shard } => return Action::Nothing(format!("shard {} does not answer: no change while a shard is down", shard)),
                ScaleAdvice::AddShard { reason } => return self.add(reason.clone()),
                ScaleAdvice::RemoveShard { reason } => return self.remove(reason.clone()),
                ScaleAdvice::Rebalance { reason } => return self.rebalance(reason.clone()),
                ScaleAdvice::Bottleneck { .. } => {}
            }
        }
        Action::Nothing("within the thresholds".into())
    }

    /// Take a reading of the cluster and act on what has been sustained. Call it every interval, from a thread that is not one
    /// of the cluster's runtime.
    pub fn step(&mut self) -> Action {
        let t = self.cluster.telemetry();
        self.observe(t)
    }

    fn changed(&mut self) {
        self.last_change = Some(Instant::now());
        // (the readings from before the change describe a cluster that is gone)
        self.history.clear();
    }

    fn add(&mut self, reason: String) -> Action {
        let addr = match self.provisioner.add_node() {
            Ok(a) => a,
            Err(e) => return Action::Failed(format!("cannot start a node: {}", e)),
        };
        match self.cluster.add_shard(&addr) {
            Ok(shard) => {
                self.changed();
                Action::Added { shard, addr, reason }
            }
            Err(e) => {
                // (the node is of no use: take it away again)
                let _ = self.provisioner.remove_node(&addr);
                Action::Failed(format!("cannot add the shard at {}: {}", addr, e.message))
            }
        }
    }

    fn remove(&mut self, reason: String) -> Action {
        let Some(addr) = self.cluster.shard_list().last().map(|s| s.addr.clone()) else { return Action::Nothing("no shards".into()) };
        if let Err(e) = self.cluster.remove_last_shard() {
            return Action::Failed(format!("cannot drain the last shard: {}", e.message));
        }
        self.changed();
        match self.provisioner.remove_node(&addr) {
            Ok(()) => Action::Removed { addr, reason },
            Err(e) => Action::Failed(format!("the shard at {} is drained and forgotten, but its node could not be removed: {}", addr, e)),
        }
    }

    fn rebalance(&mut self, reason: String) -> Action {
        match self.cluster.rebalance() {
            Ok(moved) => {
                if moved > 0 {
                    self.changed();
                }
                Action::Rebalanced { moved, reason }
            }
            Err(e) => Action::Failed(format!("cannot rebalance: {}", e.message)),
        }
    }
}
