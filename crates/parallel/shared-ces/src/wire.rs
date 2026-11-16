//! The messages between a control node and a shard node.
//!
//! The data plane (result sets) travels as Arrow Flight `DoGet` streams (see `arrowconv`); everything else is a Flight
//! action whose body is one of these types encoded with postcard (the engine's own serialization, so a statement
//! tree needs no printer: the control node sends the rewritten syntax tree and the shard runs it).
use serde::{Deserialize, Serialize};
use tsql_server::ast::Stmt;
use tsql_server::SqlError;

/// Flight action names.
pub const ACTION_RUN: &str = "run";
pub const ACTION_CLOSE_SESSION: &str = "close_session";
pub const ACTION_STATUS: &str = "status";
/// A Raft message between the replicas of a shard.
pub const ACTION_RAFT: &str = "raft";

/// The error number of "this replica is not the leader": the message is `NOTLEADER|<address of the leader or empty>`.
pub const NOT_LEADER: i32 = 50200;

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
