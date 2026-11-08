//! Error types and control-flow signals.
use std::fmt;

#[derive(Clone, Debug)]
pub struct SqlError {
    pub number: i32,
    pub severity: u8,
    pub state: u8,
    pub message: String,
    pub abort_batch: bool,
    pub procedure: Option<String>,
}

pub type Result<T> = std::result::Result<T, SqlError>;

impl SqlError {
    pub fn new(number: i32, message: impl Into<String>) -> Self {
        SqlError { number, severity: 16, state: 1, message: message.into(), abort_batch: false, procedure: None }
    }
    pub fn with(number: i32, severity: u8, state: u8, message: impl Into<String>) -> Self {
        SqlError { number, severity, state, message: message.into(), abort_batch: false, procedure: None }
    }
    pub fn abort(mut self) -> Self {
        self.abort_batch = true;
        self
    }
    pub fn syntax(near: &str) -> Self {
        SqlError::new(102, format!("Incorrect syntax near '{}'.", near)).abort()
    }
    pub fn syntax_eof() -> Self {
        SqlError::new(102, "Incorrect syntax near the end of the batch.".to_string()).abort()
    }
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Msg {}, Level {}, State {}: {}", self.number, self.severity, self.state, self.message)
    }
}
impl std::error::Error for SqlError {}

/// Non-local control flow used by the statement executor.
#[derive(Debug)]
pub enum Interrupt {
    Error(SqlError),
    Break,
    Continue,
    Return,
    Goto(String),
}

impl From<SqlError> for Interrupt {
    fn from(e: SqlError) -> Self {
        Interrupt::Error(e)
    }
}

pub type Flow<T> = std::result::Result<T, Interrupt>;

#[macro_export]
macro_rules! err {
    ($num:expr, $($arg:tt)*) => { $crate::error::SqlError::new($num, format!($($arg)*)) };
}
