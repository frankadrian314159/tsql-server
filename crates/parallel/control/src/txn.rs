//! Transactions across shards.
//!
//! `BEGIN TRANSACTION` on a control session begins nothing yet; the transaction begins on a shard the first time a
//! statement uses it, on that shard's session for this control session (so its locks and its uncommitted changes are held
//! there, and the session reads its own writes). Isolation and locking are the shard engines': what a statement does on a
//! shard it does under the usual locks, and a transaction that waits for a lock fails after the lock timeout
//! ([`Cluster::set_lock_timeout_ms`]), which also breaks a deadlock between shards (neither shard sees the cycle).
//!
//! `COMMIT`: a transaction that touched one shard is committed there (atomic by the engine); one that touched several runs
//! **two-phase commit**:
//!
//! 1. *prepare*: every participant makes its part of the transaction durable under a transaction id (the engine writes a
//!    `Prepare` record, with the changes the commit will make, to its log, which a replicated shard puts in its group's log)
//!    and keeps the transaction open with its locks. If any participant cannot, all are rolled back;
//! 2. *decision*: the decision to commit is recorded in the control node's state file (with the participants), then every
//!    participant is told to settle the transaction as committed. A participant commits from its open transaction, or, if it
//!    restarted or failed over since it voted, from its durable `Prepare` record, so a vote is never lost. A participant that
//!    cannot be reached keeps the transaction (in doubt, refusing statements until settled) and the commit is finished by
//!    [`Cluster::resolve_pending`]; a shard that meets a statement while in doubt is settled on the spot with the verdict
//!    of this node (committed if its decision was recorded, else rolled back: presumed abort), and a control node opened on
//!    its state file settles everything the shards hold ([`Cluster::settle_prepared`]).
//!
//! What this does not do: a participant fails during phase 1 only fails the transaction (nothing was decided); the window
//! between a vote and the commit is covered by the durable prepare. The state file is the coordinator's one point of
//! failure: a control node that loses it cannot tell the shards how its transactions were decided.
use crate::*;

fn no_txn(what: &str) -> SqlError {
    SqlError::new(if what == "COMMIT" { 3902 } else { 3903 }, format!("The {} TRANSACTION request has no corresponding BEGIN TRANSACTION.", what))
}

impl ClusterSession {
    fn bump_written(&self, tx: &TxnState) {
        let mut t = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner());
        for id in &tx.wrote {
            if let Some(e) = t.get_mut(id) {
                // what the mirror and the statistics took while the transaction was open is not what is left
                e.version += 1;
                e.loaded = None;
            }
        }
    }

    /// Roll the transaction back on every shard that began it (best effort: a shard that cannot be reached rolls it back
    /// when its session ends).
    pub(crate) async fn finish_rollback(&self, tx: &TxnState) {
        let shards: Vec<usize> = tx.shards.iter().copied().collect();
        let clients: Vec<ShardClient> = shards.iter().map(|&i| self.cluster.shard(i)).collect();
        let futs: Vec<_> = clients.iter().map(|c| c.run(self.id, Work::Sql("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION".into()))).collect();
        let _ = join_all(futs).await;
        self.bump_written(tx);
    }

    pub(crate) async fn rollback_txn(&mut self) -> Result<(), SqlError> {
        let Some(tx) = self.txn.borrow_mut().take() else { return Err(no_txn("ROLLBACK")) };
        self.finish_rollback(&tx).await;
        Ok(())
    }

    pub(crate) async fn commit_txn(&mut self, events: &mut Vec<Event>) -> Result<(), SqlError> {
        {
            let mut t = self.txn.borrow_mut();
            match t.as_mut() {
                None => return Err(no_txn("COMMIT")),
                Some(x) if x.depth > 1 => {
                    x.depth -= 1;
                    return Ok(());
                }
                _ => {}
            }
        }
        let tx = self.txn.borrow_mut().take().expect("a transaction");
        if tx.doomed {
            self.finish_rollback(&tx).await;
            return Err(SqlError::new(3930, "The current transaction cannot be committed and cannot support operations that write to the log file. Roll back the transaction."));
        }
        let shards: Vec<usize> = tx.shards.iter().copied().collect();
        let sid = self.id;
        match shards.len() {
            0 => Ok(()),
            1 => {
                let r = self.cluster.shard(shards[0]).run(sid, Work::Sql("COMMIT TRANSACTION".into())).await;
                let err = match r {
                    Ok(reply) => reply.first_error().map(|e| e.to_sql()),
                    Err(e) => Some(e),
                };
                match err {
                    None => Ok(()),
                    Some(e) => {
                        self.finish_rollback(&tx).await;
                        Err(e)
                    }
                }
            }
            _ => self.two_phase_commit(tx, shards, events).await,
        }
    }

    async fn two_phase_commit(&mut self, tx: TxnState, shards: Vec<usize>, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let sid = self.id;
        let cl = self.cluster.clone();
        let gid = cl.next_gid.fetch_add(1, Ordering::Relaxed);
        cl.preparing.lock().unwrap_or_else(|e| e.into_inner()).insert(gid);
        let clients: Vec<ShardClient> = shards.iter().map(|&i| cl.shard(i)).collect();
        // phase 1: every participant makes its part of the transaction durable (and keeps it open, with its locks)
        let votes = join_all(clients.iter().map(|c| c.prepare(sid, gid))).await;
        let mut refusal: Option<SqlError> = None;
        let mut prepared: Vec<usize> = vec![];
        for (v, s) in votes.into_iter().zip(&shards) {
            match v {
                Ok(true) => prepared.push(*s),
                Ok(false) => {}
                Err(e) => refusal = refusal.or(Some(e)),
            }
        }
        if let Some(e) = refusal {
            // nobody committed, and no decision is recorded: abort everywhere. A shard that prepared settles the transaction
            // as aborted when it rolls back; one that cannot be reached is told by its verdict when it is back.
            cl.preparing.lock().unwrap_or_else(|e| e.into_inner()).remove(&gid);
            self.finish_rollback(&tx).await;
            return Err(e);
        }
        if cl.fault("after_prepare") {
            return Err(SqlError::new(50220, "The control node stopped before deciding the transaction (a test fault)."));
        }
        // the decision: recorded before anything is committed, so that a control node that stops now finishes the commit
        if !prepared.is_empty() {
            cl.pending.lock().unwrap_or_else(|e| e.into_inner()).push((gid, prepared.clone()));
            cl.save_state();
        }
        cl.preparing.lock().unwrap_or_else(|e| e.into_inner()).remove(&gid);
        if cl.fault("after_decision") {
            return Err(SqlError::new(50220, "The control node stopped after deciding the transaction (a test fault)."));
        }
        // phase 2: a prepared shard commits from its durable record (so it commits even if it lost the open transaction
        // since it voted); a shard that changed nothing just ends its transaction
        let mut waiting: Vec<usize> = vec![];
        for (c, s) in clients.iter().zip(&shards) {
            if prepared.contains(s) {
                let mut done = false;
                for attempt in 0..3 {
                    match c.settle(gid, true).await {
                        Ok(_) => {
                            done = true;
                            break;
                        }
                        Err(_) if attempt < 2 => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
                        Err(_) => {}
                    }
                }
                if !done {
                    waiting.push(*s);
                }
            } else {
                let _ = c.run(sid, Work::Sql("IF @@TRANCOUNT > 0 COMMIT TRANSACTION".into())).await;
            }
        }
        {
            let mut p = cl.pending.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(i) = p.iter().position(|(g, _)| *g == gid) {
                if waiting.is_empty() {
                    p.remove(i);
                } else {
                    p[i].1 = waiting.clone();
                }
            }
        }
        cl.save_state();
        self.bump_written(&tx);
        if !waiting.is_empty() {
            events.push(Event::Message(format!("The transaction is committed; shard(s) {:?} have not confirmed yet and will finish it when they are reachable (Cluster::resolve_pending).", waiting)));
        }
        Ok(())
    }
}

impl Cluster {
    /// Finish the transactions whose commit was decided but not confirmed by every shard (a shard was unreachable). Returns
    /// how many are still waiting.
    pub fn resolve_pending(self: &Arc<Self>) -> usize {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for (gid, shards) in pending {
            let mut left = vec![];
            for s in shards {
                if s >= self.shard_count() {
                    continue;
                }
                if self.rt.block_on(self.shard(s).settle(gid, true)).is_err() {
                    left.push(s);
                }
            }
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(i) = p.iter().position(|(x, _)| *x == gid) {
                if left.is_empty() {
                    p.remove(i);
                } else {
                    p[i].1 = left;
                }
            }
        }
        self.save_state();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Settle every transaction the shards hold prepared, as this node decided it (committed if its commit was recorded,
    /// rolled back otherwise). Run when the control node starts, when no session of its own can be holding one; statements
    /// that meet a shard with in-doubt transactions do the same for them on the way. Returns how many were settled.
    pub fn settle_prepared(self: &Arc<Self>) -> usize {
        let mut n = 0;
        for s in self.shard_list() {
            if let Ok((done, _)) = self.rt.block_on(s.settle_prepared(true)) {
                n += done;
            }
        }
        n
    }

    /// How many transactions have a commit decided that some shard has not confirmed.
    pub fn pending_transactions(&self) -> usize {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}
