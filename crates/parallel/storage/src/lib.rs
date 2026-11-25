//! A shard node of the distributed tsql-server.
//!
//! The node is a complete single-node engine (`tsql_server::Server`: storage, write-ahead log, locks, optimizer) behind
//! an Arrow Flight service. Nothing about storage or concurrency is reimplemented here; the cluster adds only routing,
//! planning and merging above it (see the control crate).
//!
//! The service:
//! * `DoGet(ticket = Fragment)` runs the work on the session's engine connection and streams the first result set as
//!   Arrow record batches;
//! * action `run` runs the work and replies with what it did (row counts, messages, errors);
//! * action `close_session` drops a session's connection; action `status` reports the node.
use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::flight_service_server::{FlightService, FlightServiceServer};
use arrow_flight::{Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightInfo, HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaResult, Ticket};
use futures::stream::{self, BoxStream};
use futures::{StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tonic::{Request, Response, Status, Streaming};
use tsql_server::{Connection, Event, ResultSet, Server};
use tsql_shared_ces::arrowconv;
use tsql_shared_ces::wire::{self, status_for, Fragment, PrepareReq, RunReply, SettleReq, WireError, WireEvent, Work, ACTION_CLOSE_SESSION, ACTION_PREPARE, ACTION_KILL, ACTION_MEMBERS, ACTION_PREPARED, ACTION_RAFT, ACTION_RUN, ACTION_RUN_SETS, ACTION_SETTLE, ACTION_STATUS, ACTION_WAITS, IN_DOUBT, MEMBERSHIP, NOT_LEADER};
use tsql_shared_ces::wire::MemberReq;
use tsql_shared_ces::wire::{LoadReq, ShuffleReq, ACTION_LOAD, ACTION_METRICS, ACTION_SHUFFLE, SHUTTING_DOWN};
use tsql_shared_ces::metrics::{NodeMetrics, Sampler};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tsql_shared_ces::route::{key_hash, slot_of};

pub mod replica;
mod shuffle;
mod transport;
pub use replica::{Replica, ReplicaConfig};

/// Rows per Arrow batch.
const BATCH_ROWS: usize = 8192;

pub struct ShardNode {
    shard_id: u32,
    server: Arc<Server>,
    /// set when this node is one replica of a replicated shard
    replica: Option<Arc<Replica>>,
    sessions: Arc<Mutex<HashMap<u64, Arc<Mutex<Connection>>>>>,
    /// the session that holds each prepared transaction that is still open (by transaction id)
    prepared_by: Arc<Mutex<HashMap<u64, u64>>>,
    /// the engine's session id of each session (readable while the session's statement runs, which holds its connection)
    spids: Mutex<HashMap<u64, i32>>,
    /// connections to the other shards (made on first use), for the shuffle
    peers: shuffle::Peers,
    /// resource use of the machine, sampled in the background
    sampler: Sampler,
    started: std::time::Instant,
    /// statements being run, and run since the start
    in_flight: Arc<AtomicU64>,
    statements: Arc<AtomicU64>,
    /// set while the node drains for a shutdown: statements of sessions with no open transaction are refused
    draining: Arc<AtomicBool>,
}

/// The sampling interval of the resource metrics.
const SAMPLE_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// A statement being run (counted in `in_flight` until it ends).
struct InFlight(Arc<AtomicU64>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What a drain did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DrainReport {
    /// transactions open when the drain began, and how many of them finished by themselves
    pub open_at_start: usize,
    pub finished: usize,
    /// transactions rolled back at the end of the grace period
    pub rolled_back: usize,
    /// prepared (two-phase commit) transactions left as they are: the log keeps them and their coordinator settles them
    pub prepared_left: usize,
    /// a checkpoint was written (the next start needs no recovery)
    pub checkpointed: bool,
    pub waited_ms: u64,
}

/// The parts of a node a process needs to stop it gracefully, kept after the node is handed to the server.
#[derive(Clone)]
pub struct NodeControl {
    server: Arc<Server>,
    sessions: Arc<Mutex<HashMap<u64, Arc<Mutex<Connection>>>>>,
    prepared_by: Arc<Mutex<HashMap<u64, u64>>>,
    draining: Arc<AtomicBool>,
    in_flight: Arc<AtomicU64>,
    statements: Arc<AtomicU64>,
    sampler: Sampler,
    started: std::time::Instant,
}

/// Whether the session's connection has a transaction open (None: the connection is busy running a statement).
fn open_transaction(conn: &Arc<Mutex<Connection>>) -> Option<bool> {
    let mut c = conn.try_lock().ok()?;
    let r = c.execute("SELECT @@TRANCOUNT");
    Some(r.resultsets().first().and_then(|rs| rs.rows.first()).and_then(|r| r.first()).map(|v| v.to_string() != "0").unwrap_or(false))
}

impl NodeControl {
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    /// The node's resource use and engine counters.
    pub fn metrics(&self) -> NodeMetrics {
        let (waits, waited) = self.server.lock_waits();
        NodeMetrics {
            sample: self.sampler.sample(),
            sessions: self.sessions.lock().unwrap_or_else(|e| e.into_inner()).len(),
            in_flight: self.in_flight.load(Ordering::SeqCst),
            statements: self.statements.load(Ordering::SeqCst),
            lock_waits: waits,
            lock_wait_secs: waited.as_secs_f64(),
            log_forces: self.server.log_forces(),
            draining: self.draining.load(Ordering::SeqCst),
            uptime_secs: self.started.elapsed().as_secs(),
        }
    }

    fn open_sessions(&self) -> Vec<(u64, Arc<Mutex<Connection>>)> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    /// Stop taking new work, let the open transactions finish (for at most `grace`), roll back what is still open, force the log
    /// and write a checkpoint. A prepared transaction is left alone: it is durable, and its coordinator settles it after the
    /// restart. The node still answers requests of sessions that have a transaction open, so that it can be committed.
    pub async fn drain(&self, grace: std::time::Duration) -> DrainReport {
        let t0 = std::time::Instant::now();
        self.draining.store(true, Ordering::SeqCst);
        let prepared_sessions = |this: &NodeControl| -> std::collections::HashSet<u64> { this.prepared_by.lock().unwrap_or_else(|e| e.into_inner()).values().copied().collect() };
        let count_open = |this: &NodeControl| -> usize {
            let prepared = prepared_sessions(this);
            this.open_sessions().iter().filter(|(id, c)| !prepared.contains(id) && open_transaction(c).unwrap_or(true)).count()
        };
        let mut report = DrainReport { open_at_start: count_open(self), ..Default::default() };
        loop {
            let open = count_open(self);
            if (open == 0 && self.in_flight.load(Ordering::SeqCst) == 0) || t0.elapsed() >= grace {
                report.finished = report.open_at_start.saturating_sub(open);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // what is still open is rolled back (a client that does not come back to finish loses its transaction)
        let prepared = prepared_sessions(self);
        for (id, conn) in self.open_sessions() {
            if prepared.contains(&id) {
                continue;
            }
            if let Ok(mut c) = conn.try_lock() {
                if open_transaction_locked(&mut c) {
                    let _ = c.execute("ROLLBACK TRANSACTION");
                    report.rolled_back += 1;
                }
            }
        }
        report.prepared_left = self.server.in_doubt().len() + prepared.len();
        let server = self.server.clone();
        report.checkpointed = tokio::task::spawn_blocking(move || {
            let _ = server.flush_log();
            server.checkpoint();
            true
        })
        .await
        .unwrap_or(false);
        report.waited_ms = t0.elapsed().as_millis() as u64;
        report
    }
}

fn open_transaction_locked(c: &mut Connection) -> bool {
    let r = c.execute("SELECT @@TRANCOUNT");
    r.resultsets().first().and_then(|rs| rs.rows.first()).and_then(|r| r.first()).map(|v| v.to_string() != "0").unwrap_or(false)
}

impl ShardNode {
    pub fn new(shard_id: u32, server: Server) -> ShardNode {
        ShardNode::build(shard_id, Arc::new(server), None)
    }

    /// A node that is one replica of a replicated shard: `server` must be a persistent engine; the replica keeps its Raft log
    /// in `cfg.dir`. Must be called inside a tokio runtime.
    pub fn replicated(shard_id: u32, server: Server, cfg: ReplicaConfig) -> std::io::Result<ShardNode> {
        let server = Arc::new(server);
        let replica = Replica::start(cfg, server.clone())?;
        Ok(ShardNode::build(shard_id, server, Some(replica)))
    }

    fn build(shard_id: u32, server: Arc<Server>, replica: Option<Arc<Replica>>) -> ShardNode {
        ShardNode {
            shard_id,
            server,
            replica,
            sessions: Default::default(),
            prepared_by: Default::default(),
            spids: Mutex::new(HashMap::new()),
            peers: Default::default(),
            sampler: Sampler::start(SAMPLE_EVERY),
            started: std::time::Instant::now(),
            in_flight: Arc::new(AtomicU64::new(0)),
            statements: Arc::new(AtomicU64::new(0)),
            draining: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The handle with which a process drains and stops the node (take it before the node is given to `spawn`).
    pub fn control(&self) -> NodeControl {
        NodeControl {
            server: self.server.clone(),
            sessions: self.sessions.clone(),
            prepared_by: self.prepared_by.clone(),
            draining: self.draining.clone(),
            in_flight: self.in_flight.clone(),
            statements: self.statements.clone(),
            sampler: self.sampler.clone(),
            started: self.started,
        }
    }

    /// The node's resource use and engine counters.
    pub fn metrics(&self) -> NodeMetrics {
        self.control().metrics()
    }

    /// Let a statement of this session run: not while the node drains, except for a session that has a transaction open (which
    /// must be able to finish it). The statement counts as in flight until the guard is dropped.
    fn admit(&self, session: u64) -> Result<InFlight, Status> {
        if self.draining.load(Ordering::SeqCst) {
            let conn = self.sessions.lock().unwrap_or_else(|e| e.into_inner()).get(&session).cloned();
            if !conn.map(|c| open_transaction(&c).unwrap_or(true)).unwrap_or(false) {
                return Err(status_for(&WireError { number: SHUTTING_DOWN, severity: 16, state: 1, message: format!("Shard {} is shutting down and takes no new work.", self.shard_id) }));
            }
        }
        self.statements.fetch_add(1, Ordering::SeqCst);
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        Ok(InFlight(self.in_flight.clone()))
    }

    pub fn replica(&self) -> Option<&Arc<Replica>> {
        self.replica.as_ref()
    }

    /// A replicated shard serves requests on its leader only; any other replica says where the leader is.
    fn check_leader(&self) -> Result<(), Status> {
        if let Some(r) = &self.replica {
            if !r.can_serve() {
                let hint = r.leader_addr().unwrap_or_default();
                return Err(status_for(&WireError { number: NOT_LEADER, severity: 16, state: 1, message: format!("NOTLEADER|{}", hint) }));
            }
        }
        Ok(())
    }

    /// Whether statements may run: this is the leader (or not replicated), and no prepared transaction is in doubt (one that
    /// voted before this node restarted or took over: what it would change is unsettled, so nothing may read or change it
    /// until its coordinator says whether it committed).
    fn check_serving(&self) -> Result<(), Status> {
        self.check_leader()?;
        let d = self.server.in_doubt();
        if !d.is_empty() {
            return Err(status_for(&WireError { number: IN_DOUBT, severity: 16, state: 1, message: format!("Shard {} has in-doubt transactions {:?}: their coordinator must settle them before statements run.", self.shard_id, d) }));
        }
        Ok(())
    }

    pub fn server(&self) -> &Arc<Server> {
        &self.server
    }

    /// Insert rows into a table of the session (what a shuffle sends to this shard).
    async fn load_rows(&self, session: u64, table: String, rows: Vec<Vec<tsql_server::Value>>) -> Result<(), Status> {
        let conn = self.session(session);
        tokio::task::spawn_blocking(move || -> Result<(), WireError> {
            let mut c = conn.lock().unwrap_or_else(|e| e.into_inner());
            // (the rows go in as committed rows, without the machinery of an INSERT: a durable shard or a session in a transaction
            // takes the ordinary INSERT, see `Connection::load_rows`)
            // (`schema.name` for a table outside dbo, as a restore sends; a shuffle's temporary tables have one-part names)
            let name: Vec<String> = table.split('.').map(|p| p.to_string()).collect();
            let mut it = rows.into_iter();
            loop {
                let part: Vec<Vec<tsql_server::Value>> = it.by_ref().take(20_000).collect();
                if part.is_empty() {
                    break;
                }
                c.load_rows(&name, None, part).map_err(|e| WireError::from_sql(&e))?;
            }
            Ok(())
        })
        .await
        .map_err(|e| Status::internal(format!("the load panicked: {}", e)))?
        .map_err(|e| status_for(&e))
    }

    /// Run the scan and send every row to the shard that owns it (or to all of them), straight to that shard.
    async fn shuffle(&self, req: ShuffleReq) -> Result<u64, Status> {
        let conn = self.session(req.session);
        let scan = Work::Stmts(vec![req.scan.clone()]);
        let rs = tokio::task::spawn_blocking(move || {
            let mut c = conn.lock().unwrap_or_else(|e| e.into_inner());
            first_result_set(&mut c, &scan)
        })
        .await
        .map_err(|e| Status::internal(format!("the scan panicked: {}", e)))?
        .map_err(|e| status_for(&e))?;
        let n = req.peers.len();
        let mut parts: Vec<Vec<Vec<tsql_server::Value>>> = vec![vec![]; n];
        let total = rs.rows.len() as u64;
        for row in rs.rows {
            match req.key {
                None => {
                    for p in parts.iter_mut() {
                        p.push(row.clone());
                    }
                }
                Some((col, class)) => {
                    let h = key_hash(&row[col], class).unwrap_or(0);
                    let dest = *req.slots.get(slot_of(h)).ok_or_else(|| Status::invalid_argument("bad slot map"))? as usize;
                    parts.get_mut(dest).ok_or_else(|| Status::invalid_argument("bad slot map"))?.push(row);
                }
            }
        }
        let sends = parts.into_iter().enumerate().filter(|(_, p)| !p.is_empty()).map(|(i, p)| {
            let (table, session, me, addr) = (req.table.clone(), req.session, req.me, req.peers[i].clone());
            async move {
                if i == me {
                    self.load_rows(session, table, p).await
                } else {
                    self.peers.send_load(&addr, LoadReq { session, table, rows: p }).await
                }
            }
        });
        for r in futures::future::join_all(sends).await {
            r?;
        }
        Ok(total)
    }

    fn session(&self, id: u64) -> Arc<Mutex<Connection>> {
        let mut m = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(id)
            .or_insert_with(|| {
                let c = self.server.connect();
                self.spids.lock().unwrap_or_else(|e| e.into_inner()).insert(id, c.spid());
                Arc::new(Mutex::new(c))
            })
            .clone()
    }

    pub fn into_service(self) -> FlightServiceServer<ShardNode> {
        FlightServiceServer::new(self)
            .max_decoding_message_size(128 * 1024 * 1024)
            .max_encoding_message_size(128 * 1024 * 1024)
    }
}

fn run_work(conn: &mut Connection, work: &Work) -> tsql_server::BatchResult {
    match work {
        Work::Sql(sql) => conn.execute(sql),
        Work::Stmts(stmts) => conn.execute_stmts(stmts),
    }
}

/// The first result set of the work, or its first error.
fn first_result_set(conn: &mut Connection, work: &Work) -> Result<ResultSet, WireError> {
    let res = run_work(conn, work);
    if let Some(e) = res.first_error() {
        return Err(WireError::from_sql(e));
    }
    for ev in res.events {
        if let Event::Rows(rs) = ev {
            return Ok(rs);
        }
    }
    Err(WireError { number: 50000, severity: 16, state: 1, message: "the statement returned no result set".into() })
}

fn summarize(res: &tsql_server::BatchResult) -> RunReply {
    let events = res
        .events
        .iter()
        .map(|e| match e {
            Event::Rows(rs) => WireEvent::Rows { columns: rs.columns.len(), rows: rs.rows.len() },
            Event::Message(m) => WireEvent::Message(m.clone()),
            Event::RowCount(n) => WireEvent::RowCount(*n),
            Event::Error(err) => WireEvent::Error(WireError::from_sql(err)),
        })
        .collect();
    RunReply { events }
}

type FlightStream<T> = BoxStream<'static, Result<T, Status>>;

#[tonic::async_trait]
impl FlightService for ShardNode {
    type HandshakeStream = FlightStream<HandshakeResponse>;
    type ListFlightsStream = FlightStream<FlightInfo>;
    type DoGetStream = FlightStream<FlightData>;
    type DoPutStream = FlightStream<PutResult>;
    type DoActionStream = FlightStream<arrow_flight::Result>;
    type ListActionsStream = FlightStream<ActionType>;
    type DoExchangeStream = FlightStream<FlightData>;

    async fn do_get(&self, request: Request<Ticket>) -> Result<Response<Self::DoGetStream>, Status> {
        self.check_serving()?;
        let frag: Fragment = postcard::from_bytes(&request.into_inner().ticket).map_err(|e| Status::invalid_argument(format!("bad ticket: {}", e)))?;
        let _busy = self.admit(frag.session)?;
        let conn = self.session(frag.session);
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let (schema_tx, schema_rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let mut c = conn.lock().unwrap_or_else(|e| e.into_inner());
            let rs = match first_result_set(&mut c, &frag.work) {
                Ok(rs) => rs,
                Err(e) => {
                    let _ = schema_tx.send(Err(status_for(&e)));
                    return;
                }
            };
            let (schema, kinds) = match arrowconv::to_schema(&rs) {
                Ok(s) => s,
                Err(e) => {
                    let _ = schema_tx.send(Err(Status::internal(e)));
                    return;
                }
            };
            if schema_tx.send(Ok(schema.clone())).is_err() {
                return;
            }
            if rs.rows.is_empty() {
                return;
            }
            for chunk in rs.rows.chunks(BATCH_ROWS) {
                match arrowconv::chunk_to_batch(schema.clone(), &kinds, chunk) {
                    Ok(batch) => {
                        if tx.blocking_send(Ok(batch)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.blocking_send(Err(arrow_flight::error::FlightError::Tonic(Box::new(Status::internal(e)))));
                        break;
                    }
                }
            }
        });
        let schema = schema_rx.await.map_err(|_| Status::internal("worker panicked before schema"))??;
        let batch_stream = stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
        let out = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .build(batch_stream)
            .map_err(|e| Status::internal(e.to_string()));
        Ok(Response::new(out.boxed()))
    }

    async fn do_action(&self, request: Request<Action>) -> Result<Response<Self::DoActionStream>, Status> {
        let action = request.into_inner();
        let body: Vec<u8> = match action.r#type.as_str() {
            ACTION_RUN => {
                self.check_serving()?;
                let frag: Fragment = postcard::from_bytes(&action.body).map_err(|e| Status::invalid_argument(format!("bad request: {}", e)))?;
                let _busy = self.admit(frag.session)?;
                let conn = self.session(frag.session);
                let reply = tokio::task::spawn_blocking(move || {
                    let mut c = conn.lock().unwrap_or_else(|e| e.into_inner());
                    summarize(&run_work(&mut c, &frag.work))
                })
                .await
                .map_err(|e| Status::internal(format!("the statement panicked: {}", e)))?;
                postcard::to_allocvec(&reply).map_err(|e| Status::internal(e.to_string()))?
            }
            ACTION_RUN_SETS => {
                self.check_serving()?;
                let frag: Fragment = postcard::from_bytes(&action.body).map_err(|e| Status::invalid_argument(format!("bad request: {}", e)))?;
                let _busy = self.admit(frag.session)?;
                let conn = self.session(frag.session);
                let reply = tokio::task::spawn_blocking(move || {
                    let mut c = conn.lock().unwrap_or_else(|e| e.into_inner());
                    let res = run_work(&mut c, &frag.work);
                    let mut sets = vec![];
                    let mut error = None;
                    for ev in res.events {
                        match ev {
                            Event::Rows(rs) => sets.push(wire::WireSet { columns: rs.columns, types: rs.types, rows: rs.rows }),
                            Event::Error(err) => {
                                error = Some(WireError::from_sql(&err));
                                break;
                            }
                            _ => {}
                        }
                    }
                    wire::SetsReply { sets, error }
                })
                .await
                .map_err(|e| Status::internal(format!("the statement panicked: {}", e)))?;
                postcard::to_allocvec(&reply).map_err(|e| Status::internal(e.to_string()))?
            }
            ACTION_METRICS => postcard::to_allocvec(&self.metrics()).map_err(|e| Status::internal(e.to_string()))?,
            ACTION_LOAD => {
                self.check_serving()?;
                let req: LoadReq = postcard::from_bytes(&action.body).map_err(|e| Status::invalid_argument(format!("bad request: {}", e)))?;
                let _busy = self.admit(req.session)?;
                self.load_rows(req.session, req.table, req.rows).await?;
                vec![]
            }
            ACTION_SHUFFLE => {
                self.check_serving()?;
                let req: ShuffleReq = postcard::from_bytes(&action.body).map_err(|e| Status::invalid_argument(format!("bad request: {}", e)))?;
                let _busy = self.admit(req.session)?;
                let sent = self.shuffle(req).await?;
                postcard::to_allocvec(&sent).map_err(|e| Status::internal(e.to_string()))?
            }
            ACTION_PREPARE => {
                self.check_leader()?;
                let req: PrepareReq = postcard::from_bytes(&action.body).map_err(|e| Status::invalid_argument(format!("bad request: {}", e)))?;
                let conn = self.session(req.session);
                let prepared = tokio::task::spawn_blocking(move || {
                    let mut c = conn.lock().unwrap_or_else(|e| e.into_inner());
                    c.prepare_transaction(req.gid)
                })
                .await
                .map_err(|e| Status::internal(format!("the prepare panicked: {}", e)))?
                .map_err(|e| status_for(&WireError::from_sql(&e)))?;
                if prepared {
                    self.prepared_by.lock().unwrap_or_else(|e| e.into_inner()).insert(req.gid, req.session);
                }
                postcard::to_allocvec(&prepared).map_err(|e| Status::internal(e.to_string()))?
            }
            ACTION_SETTLE => {
                self.check_leader()?;
                let req: SettleReq = postcard::from_bytes(&action.body).map_err(|e| Status::invalid_argument(format!("bad request: {}", e)))?;
                let server = self.server.clone();
                // a transaction still open on a session is settled by that session; one that is in doubt, by the engine
                let holder = match server.prepared_state(req.gid) {
                    Some(true) => Some(self.prepared_by.lock().unwrap_or_else(|e| e.into_inner()).get(&req.gid).copied().and_then(|s| self.sessions.lock().unwrap_or_else(|e| e.into_inner()).get(&s).cloned())),
                    _ => None,
                };
                let done = tokio::task::spawn_blocking(move || -> Result<bool, tsql_server::SqlError> {
                    match holder {
                        Some(Some(conn)) => {
                            let mut c = conn.lock().unwrap_or_else(|e| e.into_inner());
                            let res = c.execute(if req.commit { "COMMIT TRANSACTION" } else { "ROLLBACK TRANSACTION" });
                            match res.first_error() {
                                Some(e) => Err(e.clone()),
                                None => Ok(true),
                            }
                        }
                        // open, but its session is not known here (the node holding it is another life of this one)
                        Some(None) => Err(tsql_server::SqlError::new(IN_DOUBT, format!("Transaction {} is open on a session this node cannot find.", req.gid))),
                        None => server.resolve_prepared(req.gid, req.commit),
                    }
                })
                .await
                .map_err(|e| Status::internal(format!("the settlement panicked: {}", e)))?
                .map_err(|e| status_for(&WireError::from_sql(&e)))?;
                self.prepared_by.lock().unwrap_or_else(|e| e.into_inner()).remove(&req.gid);
                postcard::to_allocvec(&done).map_err(|e| Status::internal(e.to_string()))?
            }
            ACTION_PREPARED => {
                self.check_leader()?;
                let mut v: Vec<(u64, bool)> = vec![];
                let ids = self.server.in_doubt();
                for g in ids {
                    v.push((g, false));
                }
                for (g, _) in self.prepared_by.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                    if self.server.prepared_state(*g) == Some(true) {
                        v.push((*g, true));
                    }
                }
                postcard::to_allocvec(&v).map_err(|e| Status::internal(e.to_string()))?
            }
            ACTION_MEMBERS => {
                let req: MemberReq = postcard::from_bytes(&action.body).map_err(|e| Status::invalid_argument(format!("bad request: {}", e)))?;
                let Some(r) = self.replica.clone() else {
                    return Err(status_for(&WireError { number: MEMBERSHIP, severity: 16, state: 1, message: "this shard is not replicated".into() }));
                };
                // listing too: a follower may be an entry behind
                self.check_leader()?;
                let members = tokio::task::spawn_blocking(move || -> Result<Vec<replica::MemberInfo>, String> {
                    let t = std::time::Duration::from_secs(req.timeout_secs.max(1));
                    if let Some((id, addr)) = &req.add {
                        r.add_replica(*id, addr, t)?;
                    }
                    if let Some(id) = req.remove {
                        r.remove_replica(id, t)?;
                    }
                    if let Some(a) = &req.remove_addr {
                        let id = r.members().iter().find(|m| &m.1 == a).map(|m| m.0).ok_or_else(|| format!("no replica at {}", a))?;
                        r.remove_replica(id, t)?;
                    }
                    Ok(r.members())
                })
                .await
                .map_err(|e| Status::internal(format!("the membership change panicked: {}", e)))?
                .map_err(|m| status_for(&WireError { number: MEMBERSHIP, severity: 16, state: 1, message: m }))?;
                postcard::to_allocvec(&members).map_err(|e| Status::internal(e.to_string()))?
            }
            ACTION_WAITS => {
                self.check_leader()?;
                let spids = self.spids.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let by_spid: HashMap<i32, u64> = spids.iter().map(|(s, p)| (*p, *s)).collect();
                let edges: Vec<(u64, u64)> = self.server.wait_graph().into_iter().filter_map(|(w, h)| Some((*by_spid.get(&w)?, *by_spid.get(&h)?))).collect();
                postcard::to_allocvec(&edges).map_err(|e| Status::internal(e.to_string()))?
            }
            ACTION_KILL => {
                self.check_leader()?;
                let session: u64 = postcard::from_bytes(&action.body).map_err(|e| Status::invalid_argument(e.to_string()))?;
                if let Some(spid) = self.spids.lock().unwrap_or_else(|e| e.into_inner()).get(&session).copied() {
                    self.server.kill_session(spid);
                }
                vec![]
            }
            ACTION_CLOSE_SESSION => {
                let id: u64 = postcard::from_bytes(&action.body).map_err(|e| Status::invalid_argument(e.to_string()))?;
                self.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                self.spids.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                vec![]
            }
            ACTION_RAFT => {
                let env: replica::Envelope = postcard::from_bytes(&action.body).map_err(|e| Status::invalid_argument(e.to_string()))?;
                if let Some(r) = &self.replica {
                    r.deliver(env.from, env.msg);
                }
                vec![]
            }
            ACTION_STATUS => {
                let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner()).len();
                let (role, term) = match &self.replica {
                    Some(r) => {
                        let s = r.state();
                        (match s.role { tsql_raft::Role::Follower => 1, tsql_raft::Role::Candidate => 2, tsql_raft::Role::Leader => 3 }, s.term)
                    }
                    None => (0, 0),
                };
                postcard::to_allocvec(&wire::ShardStatus { shard_id: self.shard_id, sessions, lock_waits: self.server.lock_waits().0, log_forces: self.server.log_forces(), role, term }).map_err(|e| Status::internal(e.to_string()))?
            }
            other => return Err(Status::unimplemented(format!("unknown action {}", other))),
        };
        let one = arrow_flight::Result { body: body.into() };
        Ok(Response::new(stream::iter(vec![Ok(one)]).boxed()))
    }

    async fn handshake(&self, _r: Request<Streaming<HandshakeRequest>>) -> Result<Response<Self::HandshakeStream>, Status> {
        Err(Status::unimplemented("handshake"))
    }
    async fn list_flights(&self, _r: Request<Criteria>) -> Result<Response<Self::ListFlightsStream>, Status> {
        Err(Status::unimplemented("list_flights"))
    }
    async fn get_flight_info(&self, _r: Request<FlightDescriptor>) -> Result<Response<FlightInfo>, Status> {
        Err(Status::unimplemented("get_flight_info"))
    }
    async fn poll_flight_info(&self, _r: Request<FlightDescriptor>) -> Result<Response<PollInfo>, Status> {
        Err(Status::unimplemented("poll_flight_info"))
    }
    async fn get_schema(&self, _r: Request<FlightDescriptor>) -> Result<Response<SchemaResult>, Status> {
        Err(Status::unimplemented("get_schema"))
    }
    async fn do_put(&self, _r: Request<Streaming<FlightData>>) -> Result<Response<Self::DoPutStream>, Status> {
        Err(Status::unimplemented("do_put"))
    }
    async fn do_exchange(&self, _r: Request<Streaming<FlightData>>) -> Result<Response<Self::DoExchangeStream>, Status> {
        Err(Status::unimplemented("do_exchange"))
    }
    async fn list_actions(&self, _r: Request<Empty>) -> Result<Response<Self::ListActionsStream>, Status> {
        let acts = [ACTION_RUN, ACTION_RUN_SETS, ACTION_CLOSE_SESSION, ACTION_STATUS, ACTION_PREPARE, ACTION_SETTLE, ACTION_PREPARED, ACTION_WAITS, ACTION_KILL, ACTION_MEMBERS, ACTION_SHUFFLE, ACTION_LOAD, ACTION_METRICS].iter().map(|n| Ok(ActionType { r#type: n.to_string(), description: String::new() })).collect::<Vec<_>>();
        Ok(Response::new(stream::iter(acts).boxed()))
    }
}

/// A shard node that is listening (for tests and for the `tsql-shard` binary).
pub struct RunningShard {
    pub addr: SocketAddr,
    /// the node's engine (for tests and tools that look at a shard directly)
    pub server: Arc<Server>,
    pub replica: Option<Arc<Replica>>,
    control: NodeControl,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl RunningShard {
    /// The handle for draining the node.
    pub fn control(&self) -> &NodeControl {
        &self.control
    }

    /// Stop gracefully: refuse new work, give open transactions `grace` to finish, roll back the rest, force the log and
    /// checkpoint, then stop the listener (and the replica's Raft loop: the other replicas elect a new leader).
    pub async fn graceful_shutdown(self, grace: std::time::Duration) -> DrainReport {
        let report = self.control.drain(grace).await;
        self.shutdown().await;
        report
    }

    /// Stop the listener and wait for it (open connections are dropped, as when a node dies).
    pub async fn shutdown(mut self) {
        if let Some(r) = &self.replica {
            r.stop();
        }
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(j) = self.join.take() {
            let _ = j.await;
        }
    }
}

/// Bind `addr` (port 0 picks a free one) and serve the node on a background task.
pub async fn spawn(node: ShardNode, addr: SocketAddr) -> std::io::Result<RunningShard> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    spawn_on(node, listener).await
}

/// Serve the node on a listener that is already bound (the replicas of a group must know each other's addresses before
/// any of them starts, so they bind first).
pub async fn spawn_on(node: ShardNode, listener: tokio::net::TcpListener) -> std::io::Result<RunningShard> {
    let local = listener.local_addr()?;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let incoming = tonic::transport::server::TcpIncoming::from(listener).with_nodelay(Some(true));
    let server = node.server().clone();
    let replica = node.replica().cloned();
    let control = node.control();
    let svc = node.into_service();
    let join = tokio::spawn(async move {
        // (many sessions share one connection from the control node: lift the limits that would otherwise close it under load)
        let _ = tonic::transport::Server::builder()
            .max_concurrent_streams(Some(1024))
            .http2_max_pending_accept_reset_streams(Some(4096))
            .initial_stream_window_size(Some(1 << 20))
            .initial_connection_window_size(Some(8 << 20))
            .add_service(svc)
            .serve_with_incoming_shutdown(incoming, async {
                let _ = stopped.await;
            })
            .await;
    });
    Ok(RunningShard { addr: local, server, replica, control, stop: Some(stop), join: Some(join) })
}
