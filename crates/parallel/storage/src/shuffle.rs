//! The connections from a shard to the other shards, used by the shuffle to send rows straight to the shard that owns them.
use arrow_flight::client::FlightClient;
use arrow_flight::error::FlightError;
use arrow_flight::Action;
use futures::TryStreamExt;
use std::collections::HashMap;
use std::sync::Mutex;
use tonic::transport::{Channel, Endpoint};
use tonic::Status;
use tsql_shared_ces::wire::{LoadReq, ACTION_LOAD};

/// Rows per `load` action (a message stays well under the transport's size limit).
const LOAD_ROWS: usize = 5000;

#[derive(Default)]
pub struct Peers {
    channels: Mutex<HashMap<String, Channel>>,
}

fn status_of(e: FlightError) -> Status {
    match e {
        // (the peer's error keeps its number: it is carried in the message)
        FlightError::Tonic(s) => Status::new(s.code(), s.message().to_string()),
        other => Status::unavailable(other.to_string()),
    }
}

impl Peers {
    fn channel(&self, addr: &str) -> Result<Channel, Status> {
        let mut m = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = m.get(addr) {
            return Ok(c.clone());
        }
        let ch = Endpoint::from_shared(format!("http://{}", addr))
            .map_err(|e| Status::invalid_argument(format!("bad shard address {}: {}", addr, e)))?
            .tcp_nodelay(true)
            .connect_timeout(std::time::Duration::from_secs(2))
            .connect_lazy();
        m.insert(addr.to_string(), ch.clone());
        Ok(ch)
    }

    /// Send rows to the shard at `addrs` (one address, or the comma-separated replicas of a replicated shard: the one that
    /// is the leader accepts them).
    pub async fn send_load(&self, addrs: &str, req: LoadReq) -> Result<(), Status> {
        let mut last = Status::unavailable("a shard needs an address");
        for chunk in req.rows.chunks(LOAD_ROWS) {
            let body = postcard::to_allocvec(&LoadReq { session: req.session, table: req.table.clone(), rows: chunk.to_vec() }).map_err(|e| Status::internal(e.to_string()))?;
            let mut done = false;
            for a in addrs.split(',').map(str::trim).filter(|a| !a.is_empty()) {
                let mut c = FlightClient::new(self.channel(a)?);
                let r = async {
                    let mut results = c.do_action(Action::new(ACTION_LOAD, body.clone())).await.map_err(status_of)?;
                    while results.try_next().await.map_err(status_of)?.is_some() {}
                    Ok::<(), Status>(())
                }
                .await;
                match r {
                    Ok(()) => {
                        done = true;
                        break;
                    }
                    Err(e) => last = e,
                }
            }
            if !done {
                return Err(last);
            }
        }
        Ok(())
    }
}
