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
use tsql_shared_ces::wire::{self, status_for, Fragment, PrepareReq, RunReply, SettleReq, WireError, WireEvent, Work, ACTION_CLOSE_SESSION, ACTION_PREPARE, ACTION_KILL, ACTION_MEMBERS, ACTION_PREPARED, ACTION_RAFT, ACTION_RUN, ACTION_SETTLE, ACTION_STATUS, ACTION_WAITS, IN_DOUBT, MEMBERSHIP, NOT_LEADER};
use tsql_shared_ces::wire::MemberReq;

pub mod replica;
mod transport;
pub use replica::{Replica, ReplicaConfig};

/// Rows per Arrow batch.
const BATCH_ROWS: usize = 8192;

pub struct ShardNode {
    shard_id: u32,
    server: Arc<Server>,
    /// set when this node is one replica of a replicated shard
    replica: Option<Arc<Replica>>,
    sessions: Mutex<HashMap<u64, Arc<Mutex<Connection>>>>,
    /// the session that holds each prepared transaction that is still open (by transaction id)
    prepared_by: Mutex<HashMap<u64, u64>>,
    /// the engine's session id of each session (readable while the session's statement runs, which holds its connection)
    spids: Mutex<HashMap<u64, i32>>,
}

impl ShardNode {
    pub fn new(shard_id: u32, server: Server) -> ShardNode {
        ShardNode { shard_id, server: Arc::new(server), replica: None, sessions: Mutex::new(HashMap::new()), prepared_by: Mutex::new(HashMap::new()), spids: Mutex::new(HashMap::new()) }
    }

    /// A node that is one replica of a replicated shard: `server` must be a persistent engine; the replica keeps its Raft log
    /// in `cfg.dir`. Must be called inside a tokio runtime.
    pub fn replicated(shard_id: u32, server: Server, cfg: ReplicaConfig) -> std::io::Result<ShardNode> {
        let server = Arc::new(server);
        let replica = Replica::start(cfg, server.clone())?;
        Ok(ShardNode { shard_id, server, replica: Some(replica), sessions: Mutex::new(HashMap::new()), prepared_by: Mutex::new(HashMap::new()), spids: Mutex::new(HashMap::new()) })
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
        let conn = self.session(frag.session);
        let rs = tokio::task::spawn_blocking(move || {
            let mut c = conn.lock().unwrap_or_else(|e| e.into_inner());
            first_result_set(&mut c, &frag.work)
        })
        .await
        .map_err(|e| Status::internal(format!("the statement panicked: {}", e)))?
        .map_err(|e| status_for(&e))?;
        let (schema, batches) = arrowconv::to_batches(&rs, BATCH_ROWS).map_err(Status::internal)?;
        let out = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .build(stream::iter(batches.into_iter().map(Ok)))
            .map_err(|e| Status::internal(e.to_string()));
        Ok(Response::new(out.boxed()))
    }

    async fn do_action(&self, request: Request<Action>) -> Result<Response<Self::DoActionStream>, Status> {
        let action = request.into_inner();
        let body: Vec<u8> = match action.r#type.as_str() {
            ACTION_RUN => {
                self.check_serving()?;
                let frag: Fragment = postcard::from_bytes(&action.body).map_err(|e| Status::invalid_argument(format!("bad request: {}", e)))?;
                let conn = self.session(frag.session);
                let reply = tokio::task::spawn_blocking(move || {
                    let mut c = conn.lock().unwrap_or_else(|e| e.into_inner());
                    summarize(&run_work(&mut c, &frag.work))
                })
                .await
                .map_err(|e| Status::internal(format!("the statement panicked: {}", e)))?;
                postcard::to_allocvec(&reply).map_err(|e| Status::internal(e.to_string()))?
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
        let acts = [ACTION_RUN, ACTION_CLOSE_SESSION, ACTION_STATUS, ACTION_PREPARE, ACTION_SETTLE, ACTION_PREPARED, ACTION_WAITS, ACTION_KILL, ACTION_MEMBERS].iter().map(|n| Ok(ActionType { r#type: n.to_string(), description: String::new() })).collect::<Vec<_>>();
        Ok(Response::new(stream::iter(acts).boxed()))
    }
}

/// A shard node that is listening (for tests and for the `tsql-shard` binary).
pub struct RunningShard {
    pub addr: SocketAddr,
    /// the node's engine (for tests and tools that look at a shard directly)
    pub server: Arc<Server>,
    pub replica: Option<Arc<Replica>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl RunningShard {
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
    let svc = node.into_service();
    let join = tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming_shutdown(incoming, async {
                let _ = stopped.await;
            })
            .await;
    });
    Ok(RunningShard { addr: local, server, replica, stop: Some(stop), join: Some(join) })
}
