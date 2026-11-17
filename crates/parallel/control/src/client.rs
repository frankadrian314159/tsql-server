//! The control node's view of one shard: Arrow Flight calls, with the shard's errors turned back into SQL errors.
use arrow_flight::client::FlightClient;
use arrow_flight::error::FlightError;
use arrow_flight::{Action, Ticket};
use futures::TryStreamExt;
use tonic::transport::{Channel, Endpoint};
use tsql_server::{ResultSet, SqlError};
use tsql_shared_ces::arrowconv;
use tsql_shared_ces::wire::{sql_error_of, Fragment, PrepareReq, RunReply, SettleReq, ShardStatus, Work, ACTION_CLOSE_SESSION, ACTION_KILL, ACTION_MEMBERS, ACTION_PREPARE, ACTION_PREPARED, ACTION_RUN, ACTION_SETTLE, ACTION_STATUS, ACTION_WAITS, IN_DOUBT, NOT_LEADER};
use tsql_shared_ces::wire::MemberReq;
use tsql_shared_ces::wire::{ShuffleReq, ACTION_METRICS, ACTION_SHUFFLE};
use tsql_shared_ces::metrics::NodeMetrics;

/// What became of a transaction that a shard holds prepared (decided by the coordinator, see `txn.rs`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    Commit,
    Abort,
    /// the coordinator is deciding it right now
    Wait,
}

pub type VerdictFn = std::sync::Arc<dyn Fn(u64) -> Verdict + Send + Sync>;

/// One shard: a single node, or the replicas of a replicated shard (whose leader serves the requests).
#[derive(Clone)]
pub struct ShardClient {
    pub id: u32,
    /// the addresses of the replicas, separated by commas (as given)
    pub addr: String,
    channels: Vec<(String, Channel)>,
    /// the replica that served last (the leader, as far as is known)
    current: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// how the coordinator decided a prepared transaction: lets a statement that finds the shard with in-doubt transactions
    /// (it restarted, or another replica took over) settle them and go on
    verdict: Option<VerdictFn>,
}

fn flight_error(e: FlightError) -> SqlError {
    match e {
        FlightError::Tonic(s) => sql_error_of(&s),
        other => SqlError::new(40613, format!("A shard could not be reached or failed: {}", other)),
    }
}

impl ShardClient {
    /// The connection is made on first use. Must be called inside a tokio runtime.
    pub fn new(id: u32, addr: &str) -> Result<ShardClient, SqlError> {
        let mut channels = vec![];
        for a in addr.split(',').map(str::trim).filter(|a| !a.is_empty()) {
            let ch = Endpoint::from_shared(format!("http://{}", a))
                .map_err(|e| SqlError::new(40613, format!("bad shard address {}: {}", a, e)))?
                .tcp_nodelay(true)
                .connect_timeout(std::time::Duration::from_secs(2))
                .connect_lazy();
            channels.push((a.to_string(), ch));
        }
        if channels.is_empty() {
            return Err(SqlError::new(40613, "a shard needs an address"));
        }
        Ok(ShardClient { id, addr: addr.to_string(), channels, current: Default::default(), verdict: None })
    }

    /// The same shard, settling the in-doubt transactions it finds according to `v`.
    pub fn with_verdict(mut self, v: VerdictFn) -> ShardClient {
        self.verdict = Some(v);
        self
    }

    /// Two-phase commit, phase one on this shard: make the session's transaction durable under `gid`. False: it changed
    /// nothing here.
    pub async fn prepare(&self, session: u64, gid: u64) -> Result<bool, SqlError> {
        let body = postcard::to_allocvec(&PrepareReq { session, gid }).map_err(|e| SqlError::new(50000, e.to_string()))?;
        let reply = self.action(ACTION_PREPARE, body).await?;
        postcard::from_bytes(&reply).map_err(|e| SqlError::new(50000, format!("bad reply from shard {}: {}", self.id, e)))
    }

    /// Commit or abort the prepared transaction `gid` (repeating it is harmless). False: the shard had settled it already.
    pub async fn settle(&self, gid: u64, commit: bool) -> Result<bool, SqlError> {
        let body = postcard::to_allocvec(&SettleReq { gid, commit }).map_err(|e| SqlError::new(50000, e.to_string()))?;
        let reply = self.action(ACTION_SETTLE, body).await?;
        postcard::from_bytes(&reply).map_err(|e| SqlError::new(50000, format!("bad reply from shard {}: {}", self.id, e)))
    }

    /// The transactions the shard holds prepared: (id, still open on a session).
    pub async fn prepared(&self) -> Result<Vec<(u64, bool)>, SqlError> {
        let reply = self.action(ACTION_PREPARED, vec![]).await?;
        postcard::from_bytes(&reply).map_err(|e| SqlError::new(50000, format!("bad reply from shard {}: {}", self.id, e)))
    }

    /// Settle the prepared transactions of the shard as the verdict says (`all`: also those still open on a session, which
    /// is right for a coordinator that has just started and so has no session of its own). Returns how many were settled and
    /// how many could not be (the coordinator is still deciding them).
    pub async fn settle_prepared(&self, all: bool) -> Result<(usize, usize), SqlError> {
        let Some(v) = self.verdict.clone() else { return Ok((0, 0)) };
        let (mut done, mut waiting) = (0, 0);
        for (gid, live) in self.prepared().await? {
            if live && !all {
                continue;
            }
            match v(gid) {
                Verdict::Wait => waiting += 1,
                d => {
                    self.settle(gid, d == Verdict::Commit).await?;
                    done += 1;
                }
            }
        }
        Ok((done, waiting))
    }

    /// A statement found in-doubt transactions: settle them and say whether the statement can be tried again.
    async fn after_in_doubt(&self, e: &SqlError) -> bool {
        e.number == IN_DOUBT && self.verdict.is_some() && matches!(self.settle_prepared(false).await, Ok((n, 0)) if n > 0)
    }

    /// Try a call on the replica that served last; when it is unreachable or not the leader, on the one it names or the
    /// others in turn, for a few seconds (an election takes a moment). The error of the last try is returned.
    async fn on_leader<T, F, Fut>(&self, f: F) -> Result<T, SqlError>
    where
        F: Fn(Channel) -> Fut,
        Fut: std::future::Future<Output = Result<T, SqlError>>,
    {
        use std::sync::atomic::Ordering;
        let n = self.channels.len();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(if n > 1 { 8 } else { 0 });
        let mut tries = 0usize;
        loop {
            let i = self.current.load(Ordering::Relaxed) % n;
            match f(self.channels[i].1.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let not_leader = e.number == NOT_LEADER;
                    let unreachable = e.number == 40613;
                    if n == 1 || !(not_leader || unreachable) || std::time::Instant::now() >= deadline {
                        return Err(e);
                    }
                    // the replica said who leads: go there; else the next one
                    let hint = e.message.strip_prefix("NOTLEADER|").unwrap_or("").to_string();
                    let next = self.channels.iter().position(|(a, _)| !hint.is_empty() && *a == hint).unwrap_or((i + 1) % n);
                    self.current.store(next, Ordering::Relaxed);
                    tries += 1;
                    if tries % n == 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }

    async fn action(&self, kind: &str, body: Vec<u8>) -> Result<Vec<u8>, SqlError> {
        self.on_leader(|ch| {
            let body = body.clone();
            async move {
                let mut c = FlightClient::new(ch);
                let mut results = c.do_action(Action::new(kind, body)).await.map_err(flight_error)?;
                let first = results.try_next().await.map_err(flight_error)?;
                // (read the stream to its end: dropping it half read makes the connection send a reset for the stream, and a
                // server that sees many of them under load closes the connection with ENHANCE_YOUR_CALM, error 40613
                // "ResourceExhausted: h2 protocol error")
                while results.try_next().await.map_err(flight_error)?.is_some() {}
                Ok(first.map(|b| b.to_vec()).unwrap_or_default())
            }
        })
        .await
    }

    /// Run work on the shard's session; the reply says what happened (errors are inside the reply).
    pub async fn run(&self, session: u64, work: Work) -> Result<RunReply, SqlError> {
        tsql_server::span!("ctl_shard_run");
        let body = {
            tsql_server::span!("ctl_encode");
            postcard::to_allocvec(&Fragment { session, work }).map_err(|e| SqlError::new(50000, e.to_string()))?
        };
        let reply = match self.action(ACTION_RUN, body.clone()).await {
            Err(e) if self.after_in_doubt(&e).await => self.action(ACTION_RUN, body).await?,
            r => r?,
        };
        tsql_server::span!("ctl_decode");
        postcard::from_bytes(&reply).map_err(|e| SqlError::new(50000, format!("bad reply from shard {}: {}", self.id, e)))
    }

    /// Run work and return every result set it produces, in order (the error of the work, if it failed, is returned as the error).
    pub async fn run_sets(&self, session: u64, work: Work) -> Result<Vec<ResultSet>, SqlError> {
        let body = postcard::to_allocvec(&Fragment { session, work }).map_err(|e| SqlError::new(50000, e.to_string()))?;
        let reply = self.action(tsql_shared_ces::wire::ACTION_RUN_SETS, body).await?;
        let r: tsql_shared_ces::wire::SetsReply = postcard::from_bytes(&reply).map_err(|e| SqlError::new(50000, format!("bad reply from shard {}: {}", self.id, e)))?;
        if let Some(e) = r.error {
            return Err(e.to_sql());
        }
        Ok(r.sets.into_iter().map(|s| ResultSet { columns: s.columns, types: s.types, rows: s.rows }).collect())
    }

    /// Run work and return its first result set (an error of the statement is returned as the error).
    pub async fn query(&self, session: u64, work: Work) -> Result<ResultSet, SqlError> {
        let ticket = postcard::to_allocvec(&Fragment { session, work }).map_err(|e| SqlError::new(50000, e.to_string()))?;
        match self.query_ticket(&ticket).await {
            Err(e) if self.after_in_doubt(&e).await => self.query_ticket(&ticket).await,
            r => r,
        }
    }

    async fn query_ticket(&self, ticket: &[u8]) -> Result<ResultSet, SqlError> {
        let id = self.id;
        self.on_leader(|ch| {
            let ticket = ticket.to_vec();
            async move {
                let mut c = FlightClient::new(ch);
                let mut stream = c.do_get(Ticket::new(ticket)).await.map_err(flight_error)?;
                let mut batches = vec![];
                while let Some(b) = stream.try_next().await.map_err(flight_error)? {
                    batches.push(b);
                }
                let schema = stream.schema().cloned().ok_or_else(|| SqlError::new(50000, format!("shard {} sent no schema", id)))?;
                arrowconv::from_batches(&schema, &batches).map_err(|e| SqlError::new(50000, format!("bad result from shard {}: {}", id, e)))
            }
        })
        .await
    }

    /// Have this shard run a scan and send its rows to the shards that own them (see `ShuffleReq`). Returns the rows sent.
    pub async fn shuffle(&self, req: ShuffleReq) -> Result<u64, SqlError> {
        let body = postcard::to_allocvec(&req).map_err(|e| SqlError::new(50000, e.to_string()))?;
        let reply = self.action(ACTION_SHUFFLE, body).await?;
        postcard::from_bytes(&reply).map_err(|e| SqlError::new(50000, format!("bad reply from shard {}: {}", self.id, e)))
    }

    /// The members of a replicated shard, after making the change `req` asks for (none: just list them): (node id, address,
    /// voter). The leader serves it.
    pub async fn members(&self, req: MemberReq) -> Result<Vec<(u64, String, bool)>, SqlError> {
        let body = postcard::to_allocvec(&req).map_err(|e| SqlError::new(50000, e.to_string()))?;
        let reply = self.action(ACTION_MEMBERS, body).await?;
        postcard::from_bytes(&reply).map_err(|e| SqlError::new(50000, format!("bad reply from shard {}: {}", self.id, e)))
    }

    /// Who waits for whom on this shard (control session ids: (waiter, holder)).
    pub async fn waits(&self) -> Result<Vec<(u64, u64)>, SqlError> {
        let reply = self.action(ACTION_WAITS, vec![]).await?;
        postcard::from_bytes(&reply).map_err(|e| SqlError::new(50000, format!("bad reply from shard {}: {}", self.id, e)))
    }

    /// Stop the session's statement on this shard and roll its transaction back there (a lock wait ends with error 596).
    pub async fn kill(&self, session: u64) -> Result<(), SqlError> {
        let body = postcard::to_allocvec(&session).map_err(|e| SqlError::new(50000, e.to_string()))?;
        self.action(ACTION_KILL, body).await.map(|_| ())
    }

    pub async fn close_session(&self, session: u64) {
        if let Ok(body) = postcard::to_allocvec(&session) {
            let _ = self.action(ACTION_CLOSE_SESSION, body).await;
        }
    }

    /// The shard's resource use (CPU, memory, disk, network) and engine counters.
    pub async fn metrics(&self) -> Result<NodeMetrics, SqlError> {
        let reply = self.action(ACTION_METRICS, vec![]).await?;
        postcard::from_bytes(&reply).map_err(|e| SqlError::new(50000, format!("bad reply from shard {}: {}", self.id, e)))
    }

    pub async fn status(&self) -> Result<ShardStatus, SqlError> {
        let reply = self.action(ACTION_STATUS, vec![]).await?;
        postcard::from_bytes(&reply).map_err(|e| SqlError::new(50000, e.to_string()))
    }
}
