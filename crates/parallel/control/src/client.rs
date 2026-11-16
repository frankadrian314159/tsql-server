//! The control node's view of one shard: Arrow Flight calls, with the shard's errors turned back into SQL errors.
use arrow_flight::client::FlightClient;
use arrow_flight::error::FlightError;
use arrow_flight::{Action, Ticket};
use futures::TryStreamExt;
use tonic::transport::{Channel, Endpoint};
use tsql_server::{ResultSet, SqlError};
use tsql_shared_ces::arrowconv;
use tsql_shared_ces::wire::{sql_error_of, Fragment, RunReply, ShardStatus, Work, ACTION_CLOSE_SESSION, ACTION_RUN, ACTION_STATUS, NOT_LEADER};

/// One shard: a single node, or the replicas of a replicated shard (whose leader serves the requests).
#[derive(Clone)]
pub struct ShardClient {
    pub id: u32,
    /// the addresses of the replicas, separated by commas (as given)
    pub addr: String,
    channels: Vec<(String, Channel)>,
    /// the replica that served last (the leader, as far as is known)
    current: std::sync::Arc<std::sync::atomic::AtomicUsize>,
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
        Ok(ShardClient { id, addr: addr.to_string(), channels, current: Default::default() })
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
                Ok(first.map(|b| b.to_vec()).unwrap_or_default())
            }
        })
        .await
    }

    /// Run work on the shard's session; the reply says what happened (errors are inside the reply).
    pub async fn run(&self, session: u64, work: Work) -> Result<RunReply, SqlError> {
        let body = postcard::to_allocvec(&Fragment { session, work }).map_err(|e| SqlError::new(50000, e.to_string()))?;
        let reply = self.action(ACTION_RUN, body).await?;
        postcard::from_bytes(&reply).map_err(|e| SqlError::new(50000, format!("bad reply from shard {}: {}", self.id, e)))
    }

    /// Run work and return its first result set (an error of the statement is returned as the error).
    pub async fn query(&self, session: u64, work: Work) -> Result<ResultSet, SqlError> {
        let ticket = postcard::to_allocvec(&Fragment { session, work }).map_err(|e| SqlError::new(50000, e.to_string()))?;
        let id = self.id;
        self.on_leader(|ch| {
            let ticket = ticket.clone();
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

    pub async fn close_session(&self, session: u64) {
        if let Ok(body) = postcard::to_allocvec(&session) {
            let _ = self.action(ACTION_CLOSE_SESSION, body).await;
        }
    }

    pub async fn status(&self) -> Result<ShardStatus, SqlError> {
        let reply = self.action(ACTION_STATUS, vec![]).await?;
        postcard::from_bytes(&reply).map_err(|e| SqlError::new(50000, e.to_string()))
    }
}
