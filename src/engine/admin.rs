//! Administration: `WAITFOR`, `KILL`, `sp_who`, `sp_configure`.
use super::query::Env;
use super::*;
use std::time::{Duration, Instant};

impl<'a> Engine<'a> {
    /// `WAITFOR DELAY 'hh:mm:ss[.fff]'` / `WAITFOR TIME 'hh:mm:ss'`. The session sleeps without holding the catalog
    /// (its transaction's locks stay, as in SQL Server) and stops early if it is killed.
    pub fn exec_waitfor(&mut self, time: bool, e: &Expr) -> Result<()> {
        let v = self.ev(e, &Env::empty())?;
        let t = match convert::cast(&v, &DataType::Time(7), None) {
            Ok(Value::Time(t)) => t,
            _ => return Err(SqlError::new(9815, "Waitfor delay and waitfor time cannot be a negative value or an invalid time.")),
        };
        use chrono::Timelike;
        let dur = if time {
            let now = chrono::Local::now().time();
            let target = t.num_seconds_from_midnight() as i64 * 1000 + (t.nanosecond() / 1_000_000) as i64;
            let cur = now.num_seconds_from_midnight() as i64 * 1000 + (now.nanosecond() / 1_000_000) as i64;
            let d = if target >= cur { target - cur } else { target + 86_400_000 - cur };
            Duration::from_millis(d as u64)
        } else {
            Duration::from_millis(t.num_seconds_from_midnight() as u64 * 1000 + (t.nanosecond() / 1_000_000) as u64)
        };
        let spid = self.ses.spid;
        let core = self.srv.core.clone();
        let end = Instant::now() + dur;
        self.srv.unlocked(|| {
            while Instant::now() < end && !core.is_killed(spid) {
                std::thread::sleep((end - Instant::now()).min(Duration::from_millis(25)));
            }
        });
        if self.srv.core.is_killed(spid) {
            return Err(self.kill_error().into());
        }
        Ok(())
    }

    /// `KILL spid`: the target's current statement ends with error 596 and its transaction rolls back.
    pub fn exec_kill(&mut self, e: &Expr) -> Result<()> {
        let v = self.ev(e, &Env::empty())?;
        let spid = v.as_i64().ok_or_else(|| SqlError::new(6106, "Process ID is not a valid process ID. Choose a number between 1 and 32767."))? as i32;
        if !self.is_sysadmin() {
            return Err(SqlError::with(300, 14, 1, "KILL permission was denied on object 'server', database 'master'."));
        }
        if spid == self.ses.spid {
            return Err(SqlError::with(6104, 16, 1, "Cannot use KILL to kill your own process."));
        }
        if !self.srv.core.session_exists(spid) {
            return Err(SqlError::with(6106, 16, 1, format!("Process ID {} is not a valid process ID. Choose a number between 1 and 32767.", spid)));
        }
        self.srv.core.kill_session(spid);
        Ok(())
    }

    /// `sp_who` / `sp_who2`: one row per session.
    pub fn sp_who(&mut self, long: bool) {
        let blocked_by = |s: &crate::coord::SessionInfo| -> i32 {
            let _ = s;
            0
        };
        let mut rows: Vec<Row> = vec![];
        for s in self.srv.core.session_list() {
            let mut r = vec![Value::Int(s.spid), Value::Str(s.status.into()), Value::Str(s.login.clone()), Value::Str(s.host.clone()), Value::Str(format!("{:>3}", blocked_by(&s)).trim().to_string()), Value::Str(s.db.clone()), Value::Str(s.command.to_uppercase())];
            if long {
                r.push(Value::Str(s.app.clone()));
                r.push(Value::Str(s.last_batch.format("%m/%d %H:%M:%S").to_string()));
            }
            rows.push(r);
        }
        let mut cols: Vec<String> = ["SPID", "Status", "Login", "HostName", "BlkBy", "DBName", "Command"].iter().map(|s| s.to_string()).collect();
        if long {
            cols.push("ProgramName".into());
            cols.push("LastBatch".into());
        }
        self.emit_rowset(super::query::Rowset::from_rows(&cols, rows));
    }

    /// `sp_configure ['option' [, value]]`.
    pub fn sp_configure(&mut self, args: &[(Option<String>, Value, Option<String>)]) -> Result<()> {
        let show_adv = self.srv.core.run_value("show advanced options") != 0;
        let Some(name) = args.first().map(|a| convert::to_str(&a.1)).filter(|n| !n.is_empty()) else {
            let rows: Vec<Row> = self
                .srv
                .core
                .config_list()
                .into_iter()
                .filter(|o| show_adv || !o.advanced)
                .map(|o| vec![Value::Str(o.name.into()), Value::Int(o.minimum.clamp(i32::MIN as i64, i32::MAX as i64) as i32), Value::Int(o.maximum.clamp(i32::MIN as i64, i32::MAX as i64) as i32), Value::Int(o.config as i32), Value::Int(o.run as i32)])
                .collect();
            self.emit_rowset(super::query::Rowset::from_rows(&["name".into(), "minimum".into(), "maximum".into(), "config_value".into(), "run_value".into()], rows));
            return Ok(());
        };
        // a unique prefix of an option name is enough
        let all = self.srv.core.config_list();
        let hits: Vec<_> = all.iter().filter(|o| o.name.to_lowercase().starts_with(&name.to_lowercase())).collect();
        let opt = match hits.as_slice() {
            [o] => (*o).clone(),
            [] => return Err(SqlError::with(15123, 16, 1, format!("The configuration option '{}' does not exist, or it may be an advanced option.", name))),
            _ => return Err(SqlError::with(15123, 16, 1, format!("The configuration option '{}' is ambiguous.", name))),
        };
        if opt.advanced && !show_adv {
            return Err(SqlError::with(15123, 16, 1, format!("The configuration option '{}' does not exist, or it may be an advanced option.", name)));
        }
        match args.get(1) {
            None => {
                let row = vec![Value::Str(opt.name.into()), Value::Int(opt.minimum.clamp(i32::MIN as i64, i32::MAX as i64) as i32), Value::Int(opt.maximum.clamp(i32::MIN as i64, i32::MAX as i64) as i32), Value::Int(opt.config as i32), Value::Int(opt.run as i32)];
                self.emit_rowset(super::query::Rowset::from_rows(&["name".into(), "minimum".into(), "maximum".into(), "config_value".into(), "run_value".into()], vec![row]));
            }
            Some((_, v, _)) => {
                if !self.is_sysadmin() {
                    return Err(SqlError::with(262, 14, 1, "ALTER SETTINGS permission denied in database 'master'."));
                }
                let n = v.as_i64().ok_or_else(|| SqlError::new(8114, "Error converting data type to int."))?;
                if n < opt.minimum || n > opt.maximum {
                    return Err(SqlError::with(15129, 16, 1, format!("The configuration option '{}' must be between {} and {}.", opt.name, opt.minimum, opt.maximum)));
                }
                self.srv.core.config_set(opt.name, n);
                self.emit(Event::Message("Configuration option changed. Run the RECONFIGURE statement to install.".into()));
            }
        }
        Ok(())
    }
}
