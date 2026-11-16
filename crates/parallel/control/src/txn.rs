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
//! 1. *prepare*: every participant is asked whether it still holds the transaction in good order (`XACT_STATE() = 1`: its
//!    session, its locks and its changes are there). If any says no, or cannot be reached, all are rolled back;
//! 2. *decision*: the decision to commit is recorded in the control node's state file (with the participants), then
//!    `COMMIT` is sent to each. A participant that cannot be reached keeps its transaction open (its locks held) and the
//!    commit is finished by [`Cluster::resolve_pending`], which also runs when a control node is opened on the state file.
//!
//! The one failure this does not cover: a participant that **loses its open transaction after it voted** (the shard process
//! crashes, or a replicated shard fails over, between prepare and commit). Its part of the transaction is gone while the
//! others commit; the commit reports error 50210 naming the shard. Closing that gap needs a durable prepared state in the
//! engine (the participant's redo record in its log before it votes), which is the next step for this layer.
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
    async fn finish_rollback(&self, tx: &TxnState) {
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
        // phase 1: does every participant still hold the transaction, in good order?
        let clients: Vec<ShardClient> = shards.iter().map(|&i| cl.shard(i)).collect();
        let votes = join_all(clients.iter().map(|c| c.query(sid, Work::Sql("SELECT XACT_STATE()".into())))).await;
        let mut refusal: Option<SqlError> = None;
        for (v, s) in votes.into_iter().zip(&shards) {
            match v {
                Ok(rs) if matches!(rs.rows.first().and_then(|r| r.first()), Some(Value::Int(1))) => {}
                Ok(_) => refusal = refusal.or_else(|| Some(SqlError::new(50210, format!("Shard {} no longer holds the transaction (it restarted or failed over); the transaction was rolled back.", s)))),
                Err(e) => refusal = refusal.or(Some(e)),
            }
        }
        if let Some(e) = refusal {
            self.finish_rollback(&tx).await;
            return Err(e);
        }
        // the decision: recorded before anything is committed, so that a control node that stops now finishes the commit
        cl.pending.lock().unwrap_or_else(|e| e.into_inner()).push((sid, shards.clone()));
        cl.save_state();
        // phase 2
        let mut lost: Vec<usize> = vec![];
        let mut waiting: Vec<usize> = vec![];
        for (c, s) in clients.iter().zip(&shards) {
            let mut outcome: Option<SqlError> = None;
            for attempt in 0..3 {
                match c.run(sid, Work::Sql("COMMIT TRANSACTION".into())).await {
                    Ok(reply) => match reply.first_error() {
                        None => {
                            outcome = None;
                            break;
                        }
                        Some(e) => {
                            outcome = Some(e.to_sql());
                            break;
                        }
                    },
                    Err(e) => {
                        outcome = Some(e);
                        if attempt < 2 {
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                    }
                }
            }
            match outcome {
                None => {}
                Some(e) if e.number == 3902 => lost.push(*s),
                Some(_) => waiting.push(*s),
            }
        }
        {
            let mut p = cl.pending.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(i) = p.iter().position(|(s, _)| *s == sid) {
                if waiting.is_empty() {
                    p.remove(i);
                } else {
                    p[i].1 = waiting.clone();
                }
            }
        }
        cl.save_state();
        self.bump_written(&tx);
        if !lost.is_empty() {
            return Err(SqlError::new(50210, format!("The transaction committed, but shard(s) {:?} lost their part of it before they could commit (the process restarted or failed over after it voted).", lost)));
        }
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
        for (session, shards) in pending {
            let mut left = vec![];
            for s in shards {
                if s >= self.shard_count() {
                    continue;
                }
                let r = self.rt.block_on(self.shard(s).run(session, Work::Sql("COMMIT TRANSACTION".into())));
                let done = match r {
                    Ok(reply) => reply.first_error().map(|e| e.number == 3902).unwrap_or(true),
                    Err(_) => false,
                };
                if !done {
                    left.push(s);
                }
            }
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(i) = p.iter().position(|(x, _)| *x == session) {
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

    /// How many transactions have a commit decided that some shard has not confirmed.
    pub fn pending_transactions(&self) -> usize {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}
