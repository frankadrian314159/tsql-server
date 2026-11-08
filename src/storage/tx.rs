//! Transaction state: isolation level, undo log and savepoints.
use super::{RowId, TxId};
use crate::catalog::{Database, Object, Table, Trigger, UserType};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isolation {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
    Snapshot,
}

impl Isolation {
    pub fn name(self) -> &'static str {
        match self {
            Isolation::ReadUncommitted => "READ UNCOMMITTED",
            Isolation::ReadCommitted => "READ COMMITTED",
            Isolation::RepeatableRead => "REPEATABLE READ",
            Isolation::Serializable => "SERIALIZABLE",
            Isolation::Snapshot => "SNAPSHOT",
        }
    }
    pub fn parse(s: &str) -> Option<Isolation> {
        Some(match s.trim().to_uppercase().as_str() {
            "READ UNCOMMITTED" => Isolation::ReadUncommitted,
            "READ COMMITTED" => Isolation::ReadCommitted,
            "REPEATABLE READ" => Isolation::RepeatableRead,
            "SERIALIZABLE" => Isolation::Serializable,
            "SNAPSHOT" => Isolation::Snapshot,
            _ => return None,
        })
    }
    /// sys.dm_exec_sessions.transaction_isolation_level
    pub fn level(self) -> i32 {
        match self {
            Isolation::ReadUncommitted => 1,
            Isolation::ReadCommitted => 2,
            Isolation::RepeatableRead => 3,
            Isolation::Serializable => 4,
            Isolation::Snapshot => 5,
        }
    }
}

/// Where a table lives, for undo purposes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabRef {
    Db(String, String),
    Temp(String),
    GTemp(String),
}

pub enum Undo {
    /// Pop the newest version of a slot.
    Row { tab: TabRef, rowid: RowId },
    Object { db: String, key: String, prev: Option<Object> },
    Temp { name: String, prev: Option<Table> },
    GTemp { name: String, prev: Option<Table> },
    Type { db: String, key: String, prev: Option<UserType> },
    Schema { db: String, key: String, prev: Option<String> },
    Database { key: String, prev: Option<Database> },
    Trigger { db: String, key: String, prev: Option<(String, Arc<Trigger>)> },
}

pub struct Tx {
    pub id: TxId,
    pub isolation: Isolation,
    /// Timestamp seen by SNAPSHOT transactions.
    pub snapshot_ts: Option<u64>,
    pub undo: Vec<Undo>,
    pub savepoints: Vec<(String, usize)>,
    pub implicit: bool,
    pub wrote: bool,
    pub start_ts: u64,
}

impl Tx {
    pub fn new(id: TxId, isolation: Isolation, implicit: bool, start_ts: u64) -> Tx {
        Tx {
            id,
            isolation,
            snapshot_ts: if isolation == Isolation::Snapshot { Some(start_ts) } else { None },
            undo: vec![],
            savepoints: vec![],
            implicit,
            wrote: false,
            start_ts,
        }
    }
}
