//! The messages between a control node and a shard node.
//!
//! The data plane (result sets) travels as Arrow Flight `DoGet` streams (see `arrowconv`); everything else is a Flight
//! action whose body is one of these types encoded with postcard (the engine's own serialization, so a statement
//! tree needs no printer: the control node sends the rewritten syntax tree and the shard runs it).
use serde::{Deserialize, Serialize};
use crate::route::KeyClass;
use tsql_server::ast::Stmt;
use tsql_server::{SqlError, Value};

/// Flight action names.
pub const ACTION_RUN: &str = "run";
pub const ACTION_CLOSE_SESSION: &str = "close_session";
pub const ACTION_STATUS: &str = "status";
/// A Raft message between the replicas of a shard.
pub const ACTION_RAFT: &str = "raft";

/// Two-phase commit. `prepare`: make the transaction of a session durable under an id (`PrepareReq`, reply: a `bool`, false
/// when the transaction changed nothing); `settle`: commit or abort a prepared transaction by its id (`SettleReq`, reply: a
/// `bool`, false when it was settled already); `prepared`: the ids of the prepared transactions of the shard and whether
/// each is still open on a session (reply: `Vec<(u64, bool)>`).
pub const ACTION_PREPARE: &str = "prepare";
pub const ACTION_SETTLE: &str = "settle";
pub const ACTION_PREPARED: &str = "prepared";

/// Global deadlock detection. `waits`: who waits for whom on this shard, as control session ids (reply: `Vec<(u64, u64)>`,
/// (waiter, holder)); `kill`: stop a session's statement and roll its transaction back on this shard (body: the session id),
/// which ends a lock wait it is in with error 596.
pub const ACTION_WAITS: &str = "waits";
pub const ACTION_KILL: &str = "kill";

/// Membership of a replicated shard (`MemberReq`, reply: the members, `Vec<(u64, String, bool)>`: id, address, voter). With
/// nothing to add or remove it only lists them. Served by the leader.
pub const ACTION_MEMBERS: &str = "members";

/// The shuffle (exchange) between shards. `shuffle` (`ShuffleReq`, reply: the number of rows sent, a `u64`): the shard runs a
/// scan on its session and sends each row to the shard that owns it by the hash of a column (or every row to every shard),
/// straight to that shard with the action `load` (`LoadReq`), which inserts the rows into a table of its session. The rows
/// never pass through the control node.
pub const ACTION_SHUFFLE: &str = "shuffle";
pub const ACTION_LOAD: &str = "load";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShuffleReq {
    pub session: u64,
    /// run on this shard's session; its first result set is what is sent
    pub scan: Stmt,
    /// the column (of the scan's result) and key class that decide the destination; None: every row goes to every shard
    pub key: Option<(usize, KeyClass)>,
    /// the table each shard inserts what it receives into (it exists on all of them)
    pub table: String,
    /// the shard that owns each hash slot
    pub slots: Vec<u32>,
    /// the address (or the comma-separated addresses of the replicas) of each shard, by index
    pub peers: Vec<String>,
    /// this shard's index
    pub me: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoadReq {
    pub session: u64,
    pub table: String,
    pub rows: Vec<Vec<Value>>,
}

/// The error number of a membership change that could not be made (the message says why).
pub const MEMBERSHIP: i32 = 50230;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct MemberReq {
    /// add this replica (node id, address) to the group: it is caught up as a learner, then made a voter
    pub add: Option<(u64, String)>,
    /// remove the replica with this node id ...
    pub remove: Option<u64>,
    /// ... or the one at this address
    pub remove_addr: Option<String>,
    /// how long the change may take
    pub timeout_secs: u64,
}

/// The error number of "this replica is not the leader": the message is `NOTLEADER|<address of the leader or empty>`.
pub const NOT_LEADER: i32 = 50200;

/// The error number of "this shard has in-doubt transactions": a shard that restarted (or failed over) with prepared
/// transactions refuses statements until their coordinator has settled them.
pub const IN_DOUBT: i32 = 50211;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrepareReq {
    pub session: u64,
    pub gid: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SettleReq {
    pub gid: u64,
    pub commit: bool,
}

/// What a shard is asked to run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Work {
    Sql(String),
    Stmts(Vec<Stmt>),
}

/// A request to a shard: the work, on the shard-side session of the control session `session` (created on first use, so
/// `USE`, `SET` options and temporary tables persist across the requests of one control session).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fragment {
    pub session: u64,
    pub work: Work,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WireError {
    pub number: i32,
    pub severity: u8,
    pub state: u8,
    pub message: String,
}

impl WireError {
    pub fn from_sql(e: &SqlError) -> WireError {
        WireError { number: e.number, severity: e.severity, state: e.state, message: e.message.clone() }
    }
    pub fn to_sql(&self) -> SqlError {
        let mut e = SqlError::new(self.number, self.message.clone());
        e.severity = self.severity;
        e.state = self.state;
        e
    }
}

/// What came out of a `run`; result sets are only counted (they travel by `DoGet`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum WireEvent {
    RowCount(i64),
    Message(String),
    Rows { columns: usize, rows: usize },
    Error(WireError),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct RunReply {
    pub events: Vec<WireEvent>,
}

impl RunReply {
    pub fn first_error(&self) -> Option<&WireError> {
        self.events.iter().find_map(|e| if let WireEvent::Error(w) = e { Some(w) } else { None })
    }
    /// The sum of the row counts of the statements that reported one.
    pub fn row_count(&self) -> i64 {
        self.events.iter().map(|e| if let WireEvent::RowCount(n) = e { *n } else { 0 }).sum()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShardStatus {
    pub shard_id: u32,
    pub sessions: usize,
    /// lock waits and log forces of the engine since it started (a measure of how busy the shard is)
    pub lock_waits: u64,
    pub log_forces: u64,
    /// 0: not replicated, 1: follower, 2: candidate, 3: leader
    pub role: u8,
    pub term: u64,
}

/// An error is carried in the message of a gRPC status, so that the number, severity and text survive the trip.
const ERR_PREFIX: &str = "tsqlerr:";

pub fn status_for(e: &WireError) -> tonic::Status {
    let body = serde_json::to_string(e).unwrap_or_else(|_| e.message.clone());
    tonic::Status::failed_precondition(format!("{}{}", ERR_PREFIX, body))
}

/// The SQL error a status stands for (a transport failure becomes error 40613-style "unavailable").
pub fn sql_error_of(s: &tonic::Status) -> SqlError {
    if let Some(rest) = s.message().strip_prefix(ERR_PREFIX) {
        if let Ok(w) = serde_json::from_str::<WireError>(rest) {
            return w.to_sql();
        }
    }
    SqlError::new(40613, format!("A shard could not be reached or failed ({:?}): {}", s.code(), s.message()))
}
