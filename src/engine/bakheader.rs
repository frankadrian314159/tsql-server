//! `RESTORE HEADERONLY`, `RESTORE FILELISTONLY` and `RESTORE LABELONLY`: what a backup file says about itself, as result sets with
//! SQL Server's columns (SSMS's Restore dialog reads them by name to list the backup sets of a device and the files in them).
//!
//! The facts come from this server's own backups (their header) or from a backup SQL Server wrote (its Microsoft Tape Format blocks);
//! what neither holds (the collation of the database, the log sequence numbers, the physical files) is a plausible stand-in.
use super::*;
use crate::decimal::Dec;
use chrono::NaiveDateTime;

/// What is known of one backup set.
pub(crate) struct BackupFacts {
    pub name: String,
    pub description: String,
    /// 1 database, 2 log, 5 differential
    pub kind: i32,
    pub user: String,
    pub server: String,
    pub machine: String,
    pub db: String,
    pub created: NaiveDateTime,
    pub size: i128,
    pub first_lsn: i128,
    pub last_lsn: i128,
    pub checkpoint_lsn: i128,
    pub db_backup_lsn: i128,
    pub diff_base_lsn: Option<i128>,
    pub start: NaiveDateTime,
    pub finish: NaiveDateTime,
    pub recovery_full: bool,
    pub copy_only: bool,
    pub collation: String,
    pub vendor: i32,
    pub major: i32,
    pub minor: i32,
    pub build: i32,
    pub compressed: bool,
    pub media_name: String,
    pub media_description: String,
    pub software: String,
    pub media_date: NaiveDateTime,
}

fn dt(t: NaiveDateTime) -> Value {
    Value::DateTime(t, crate::types::DtKind::DateTime)
}

fn num(v: i128) -> Value {
    Value::Dec(Dec::new(v, 0))
}

fn guid(seed: &str, what: &str) -> Value {
    // a stable identifier made from the database and what it names, so repeated reads of one file agree
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(format!("{}/{}", seed, what).as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&h[..16]);
    Value::Guid(uuid::Uuid::from_bytes(b))
}

impl<'a> Engine<'a> {
    fn emit_rows(&mut self, cols: Vec<(&str, DataType)>, rows: Vec<Row>) {
        let n = rows.len() as u64;
        self.emit(Event::Rows(ResultSet { columns: cols.iter().map(|c| c.0.to_string()).collect(), types: cols.into_iter().map(|c| c.1).collect(), rows }));
        if !self.ses.nocount {
            self.emit(Event::RowCount(n as i64));
        }
        self.ses.rowcount = n as i64;
    }

    pub(crate) fn emit_header_only(&mut self, f: &BackupFacts) {
        use DataType::*;
        let kind_desc = match f.kind {
            2 => "Transaction Log",
            5 => "Database Differential",
            _ => "Database",
        };
        let seed = f.db.clone();
        let cols: Vec<(&str, DataType)> = vec![
            ("BackupName", NVarChar(128)), ("BackupDescription", NVarChar(255)), ("BackupType", SmallInt), ("ExpirationDate", DateTime), ("Compressed", TinyInt), ("Position", SmallInt),
            ("DeviceType", TinyInt), ("UserName", NVarChar(128)), ("ServerName", NVarChar(128)), ("DatabaseName", NVarChar(128)), ("DatabaseVersion", Int), ("DatabaseCreationDate", DateTime),
            ("BackupSize", Decimal(20, 0)), ("FirstLSN", Decimal(25, 0)), ("LastLSN", Decimal(25, 0)), ("CheckpointLSN", Decimal(25, 0)), ("DatabaseBackupLSN", Decimal(25, 0)),
            ("BackupStartDate", DateTime), ("BackupFinishDate", DateTime), ("SortOrder", SmallInt), ("CodePage", SmallInt), ("UnicodeLocaleId", Int), ("UnicodeComparisonStyle", Int),
            ("CompatibilityLevel", TinyInt), ("SoftwareVendorId", Int), ("SoftwareVersionMajor", Int), ("SoftwareVersionMinor", Int), ("SoftwareVersionBuild", Int), ("MachineName", NVarChar(128)),
            ("Flags", Int), ("BindingID", Guid), ("RecoveryForkID", Guid), ("Collation", NVarChar(128)), ("FamilyGUID", Guid), ("HasBulkLoggedData", Bit), ("IsSnapshot", Bit), ("IsReadOnly", Bit),
            ("IsSingleUser", Bit), ("HasBackupChecksums", Bit), ("IsDamaged", Bit), ("BeginsLogChain", Bit), ("HasIncompleteMetaData", Bit), ("IsForceOffline", Bit), ("IsCopyOnly", Bit),
            ("FirstRecoveryForkID", Guid), ("ForkPointLSN", Decimal(25, 0)), ("RecoveryModel", NVarChar(60)), ("DifferentialBaseLSN", Decimal(25, 0)), ("DifferentialBaseGUID", Guid),
            ("BackupTypeDescription", NVarChar(60)), ("BackupSetGUID", Guid), ("CompressedBackupSize", BigInt), ("Containment", TinyInt), ("KeyAlgorithm", NVarChar(32)),
            ("EncryptorThumbprint", VarBinary(20)), ("EncryptorType", NVarChar(32)),
        ];
        let row: Row = vec![
            Value::Str(f.name.clone()), Value::Str(f.description.clone()), Value::Int(f.kind), Value::Null, Value::Int(f.compressed as i32), Value::Int(1),
            Value::Int(2), Value::Str(f.user.clone()), Value::Str(f.server.clone()), Value::Str(f.db.clone()), Value::Int(957), dt(f.created),
            num(f.size), num(f.first_lsn), num(f.last_lsn), num(f.checkpoint_lsn), num(f.db_backup_lsn),
            dt(f.start), dt(f.finish), Value::Int(52), Value::Int(1252), Value::Int(1033), Value::Int(0),
            Value::Int(160), Value::Int(f.vendor), Value::Int(f.major), Value::Int(f.minor), Value::Int(f.build), Value::Str(f.machine.clone()),
            Value::Int(512), guid(&seed, "binding"), guid(&seed, "fork"), Value::Str(f.collation.clone()), guid(&seed, "family"), Value::Bit(false), Value::Bit(false), Value::Bit(false),
            Value::Bit(false), Value::Bit(true), Value::Bit(false), Value::Bit(f.kind == 1 && !f.copy_only && f.recovery_full), Value::Bit(false), Value::Bit(false), Value::Bit(f.copy_only),
            guid(&seed, "fork"), num(0), Value::Str(if f.recovery_full { "FULL" } else { "SIMPLE" }.into()), f.diff_base_lsn.map(num).unwrap_or(Value::Null), if f.kind == 5 { guid(&seed, "diffbase") } else { Value::Null },
            Value::Str(kind_desc.into()), guid(&seed, &format!("set{}", f.first_lsn)), Value::BigInt(f.size as i64), Value::Int(0), Value::Null,
            Value::Null, Value::Null,
        ];
        self.emit_rows(cols, vec![row]);
    }

    pub(crate) fn emit_file_list_only(&mut self, f: &BackupFacts) {
        use DataType::*;
        let cols: Vec<(&str, DataType)> = vec![
            ("LogicalName", NVarChar(128)), ("PhysicalName", NVarChar(260)), ("Type", Char(1)), ("FileGroupName", NVarChar(128)), ("Size", Decimal(20, 0)), ("MaxSize", Decimal(20, 0)),
            ("FileId", BigInt), ("CreateLSN", Decimal(25, 0)), ("DropLSN", Decimal(25, 0)), ("UniqueId", Guid), ("ReadOnlyLSN", Decimal(25, 0)), ("ReadWriteLSN", Decimal(25, 0)),
            ("BackupSizeInBytes", BigInt), ("SourceBlockSize", Int), ("FileGroupId", Int), ("LogGroupGUID", Guid), ("DifferentialBaseLSN", Decimal(25, 0)), ("DifferentialBaseGUID", Guid),
            ("IsReadOnly", Bit), ("IsPresent", Bit), ("TDEThumbprint", VarBinary(32)), ("SnapshotUrl", NVarChar(360)),
        ];
        let seed = f.db.clone();
        let data_size = (f.size.max(8 * 1024 * 1024) / 8192 + 1) * 8192;
        let mut rows: Vec<Row> = vec![];
        for (i, (logical, ext, ty, group)) in [(f.db.clone(), "mdf", "D", Some("PRIMARY")), (format!("{}_log", f.db), "ldf", "L", None)].into_iter().enumerate() {
            let is_data = i == 0;
            rows.push(vec![
                Value::Str(logical.clone()), Value::Str(format!("{}.{}", logical, ext)), Value::Str(ty.into()), group.map(|g| Value::Str(g.into())).unwrap_or(Value::Null),
                num(if is_data { data_size } else { 8 * 1024 * 1024 }), num(if is_data { 35184372080640 } else { 2199023255552 }),
                Value::BigInt(i as i64 + 1), num(0), num(0), guid(&seed, &format!("file{}", i)), num(0), num(0),
                Value::BigInt(if is_data { f.size as i64 } else { 0 }), Value::Int(512), Value::Int(if is_data { 1 } else { 0 }), Value::Null, num(0), Value::Null,
                Value::Bit(false), Value::Bit(true), Value::Null, Value::Str(String::new()),
            ]);
        }
        self.emit_rows(cols, rows);
    }

    pub(crate) fn emit_label_only(&mut self, f: &BackupFacts) {
        use DataType::*;
        let cols: Vec<(&str, DataType)> = vec![
            ("MediaName", NVarChar(128)), ("MediaSetId", Guid), ("FamilyCount", Int), ("FamilySequenceNumber", Int), ("MediaFamilyId", Guid), ("MediaSequenceNumber", Int),
            ("MediaLabelPresent", TinyInt), ("MediaDescription", NVarChar(255)), ("SoftwareName", NVarChar(128)), ("SoftwareVendorId", Int), ("MediaDate", DateTime), ("Mirror_Count", Int),
            ("IsCompressed", Bit),
        ];
        let seed = f.db.clone();
        let row: Row = vec![
            if f.media_name.is_empty() { Value::Null } else { Value::Str(f.media_name.clone()) }, guid(&seed, "mediaset"), Value::Int(1), Value::Int(1), guid(&seed, "mediafamily"), Value::Int(1),
            Value::Int(1), if f.media_description.is_empty() { Value::Null } else { Value::Str(f.media_description.clone()) }, Value::Str(f.software.clone()), Value::Int(f.vendor), dt(f.media_date), Value::Int(1),
            Value::Bit(f.compressed),
        ];
        self.emit_rows(cols, vec![row]);
    }
}
