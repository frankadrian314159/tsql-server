//! Changing the replicas of a replicated shard while the cluster serves.
//!
//! A shard is a group of replicas (see `tsql-storage`); their addresses are the shard's address, separated by commas. A new
//! replica is started by the orchestrator as `tsql-shard --id N --replica R --join --peers ...` on an empty directory, and
//! [`Cluster::add_replica`] asks the shard's leader to take it into the group: it is sent the log (or the leader's snapshot),
//! and when it has caught up it becomes a voter. [`Cluster::remove_replica`] takes one out (the process can then be stopped).
//! The control node follows: its client of the shard and its state file list the new set of replicas.
use crate::*;
use tsql_shared_ces::wire::MemberReq;

impl Cluster {
    /// The replicas of shard `shard` as its group knows them: (node id, address, voter).
    pub fn replicas(&self, shard: usize) -> Result<Vec<(u64, String, bool)>, SqlError> {
        let client = self.shard_checked(shard)?;
        self.rt.block_on(client.members(MemberReq::default()))
    }

    /// Add the replica `node_id`, a `tsql-shard --join` listening at `addr`, to the group of shard `shard`. Returns when the
    /// replica has caught up and is a voter.
    pub fn add_replica(&self, shard: usize, node_id: u64, addr: &str) -> Result<(), SqlError> {
        let client = self.shard_checked(shard)?;
        let members = self.rt.block_on(client.members(MemberReq { add: Some((node_id, addr.to_string())), timeout_secs: 120, ..Default::default() }))?;
        self.follow_members(shard, &members)
    }

    /// Remove the replica at `addr` from the group of shard `shard` (the leader may be removed: the others elect another).
    pub fn remove_replica(&self, shard: usize, addr: &str) -> Result<(), SqlError> {
        let client = self.shard_checked(shard)?;
        let members = self.rt.block_on(client.members(MemberReq { remove_addr: Some(addr.to_string()), timeout_secs: 60, ..Default::default() }))?;
        self.follow_members(shard, &members)
    }

    fn shard_checked(&self, shard: usize) -> Result<ShardClient, SqlError> {
        if shard >= self.shard_count() {
            return Err(SqlError::new(50000, format!("there is no shard {}", shard)));
        }
        Ok(self.shard(shard))
    }

    /// The shard's replicas are now `members`: talk to those, and remember them.
    fn follow_members(&self, shard: usize, members: &[(u64, String, bool)]) -> Result<(), SqlError> {
        let addrs: Vec<&str> = members.iter().filter(|m| !m.1.is_empty()).map(|m| m.1.as_str()).collect();
        if addrs.is_empty() {
            return Ok(());
        }
        let id = self.shard(shard).id;
        let client = {
            let _g = self.rt.enter();
            ShardClient::new(id, &addrs.join(","))?.with_verdict(self.verdict_fn())
        };
        self.shards.write().unwrap_or_else(|e| e.into_inner())[shard] = client;
        self.save_state();
        Ok(())
    }
}
