//! What the app needs from a database connection, so that it can work on an engine inside the process (`Connection`)
//! or on a server reached over TDS (`remote::RemoteBackend`: a cluster's control node, or any SQL Server work-alike).
use tsql_server::inspect::{ObjectEntry, TableInfo};
use tsql_server::{lexer, BatchResult, Connection, SqlError, Value};

pub trait Backend: Send {
    /// Run one batch (no `GO` handling).
    fn execute(&mut self, sql: &str) -> BatchResult;
    /// Structure of a table of the current database (`name` may be `schema.name`, bracketed or not).
    fn table_info(&mut self, name: &str) -> Option<TableInfo>;
    /// Tables, views, procedures, functions and sequences of the current database.
    fn objects(&mut self) -> Vec<ObjectEntry>;
    /// The `CREATE` text of a view, procedure or function.
    fn definition(&mut self, name: &str) -> Option<String>;
    fn databases(&mut self) -> Vec<String>;
    fn current_database(&mut self) -> String;
    /// `@@TRANCOUNT` (the last value the session knew).
    fn transaction_count(&self) -> i32;
    fn login(&self) -> String;
    /// The optimizer's plan for `sql`, without running it.
    fn explain(&mut self, sql: &str) -> Result<Vec<String>, SqlError>;

    /// Run a script with `GO` separators.
    fn execute_script(&mut self, script: &str) -> Vec<BatchResult> {
        let mut out = vec![];
        for (batch, count) in lexer::split_batches(script) {
            for _ in 0..count {
                out.push(self.execute(&batch));
            }
        }
        out
    }
    /// The first result set's rows of a batch, or its first error.
    fn query(&mut self, sql: &str) -> Result<Vec<Vec<Value>>, SqlError> {
        let res = self.execute(sql);
        if let Some(e) = res.first_error() {
            return Err(e.clone());
        }
        Ok(res.resultsets().first().map(|r| r.rows.clone()).unwrap_or_default())
    }
    /// Run a batch; an error if any statement failed.
    fn run(&mut self, sql: &str) -> Result<BatchResult, SqlError> {
        let res = self.execute(sql);
        match res.first_error() {
            Some(e) => Err(e.clone()),
            None => Ok(res),
        }
    }
}

impl Backend for Connection {
    fn execute(&mut self, sql: &str) -> BatchResult {
        Connection::execute(self, sql)
    }
    fn table_info(&mut self, name: &str) -> Option<TableInfo> {
        Connection::table_info(self, name)
    }
    fn objects(&mut self) -> Vec<ObjectEntry> {
        Connection::objects(self)
    }
    fn definition(&mut self, name: &str) -> Option<String> {
        Connection::definition(self, name)
    }
    fn databases(&mut self) -> Vec<String> {
        Connection::databases(self)
    }
    fn current_database(&mut self) -> String {
        Connection::current_database(self)
    }
    fn transaction_count(&self) -> i32 {
        Connection::transaction_count(self)
    }
    fn login(&self) -> String {
        Connection::login(self)
    }
    fn explain(&mut self, sql: &str) -> Result<Vec<String>, SqlError> {
        Connection::explain(self, sql)
    }
}

/// The import functions of `tsql_server::tools` work on any backend.
impl<'a> tsql_server::tools::SqlSession for dyn Backend + 'a {
    fn run_sql(&mut self, sql: &str) -> Result<(), SqlError> {
        self.run(sql).map(|_| ())
    }
    fn describe(&mut self, name: &str) -> Option<TableInfo> {
        self.table_info(name)
    }
    fn open_transactions(&self) -> i32 {
        self.transaction_count()
    }
}
