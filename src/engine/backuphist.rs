//! The backup history of `msdb`: `backupset`, `backupmediaset`, `backupmediafamily`, `backupfile`, `restorehistory`, `restorefile`.
//!
//! The tables are real tables of the `msdb` database (so they are stored, logged, recovered, backed up and queried like any
//! others, and nothing about the storage formats changes). They are made the first time a backup or a restore is recorded;
//! until then `msdb.dbo.backupset` and the others answer as empty views (see `sysviews.rs`). Recording a backup is not allowed to
//! fail it: an error while writing the history is dropped.
//!
//! The statements that write the history run as the server itself (`Session::system_ctx`), in `msdb`, whoever took the backup:
//! a `db_backupoperator` has no rights there.
use super::*;

/// The tables, as SQL Server defines them (the columns SQL Server's tools read; a few it adds in later versions are left out).
const HISTORY_DDL: &str = "
CREATE TABLE dbo.backupmediaset (media_set_id int IDENTITY(1,1) PRIMARY KEY, media_uuid uniqueidentifier, media_family_count tinyint, name nvarchar(128), description nvarchar(255), software_name nvarchar(128), software_vendor_id int, MTF_major_version tinyint, mirror_count tinyint, is_password_protected bit, is_compressed bit);
CREATE TABLE dbo.backupmediafamily (media_set_id int NOT NULL, family_sequence_number tinyint NOT NULL, media_family_id uniqueidentifier, media_count int, logical_device_name nvarchar(128), physical_device_name nvarchar(260), device_type tinyint, physical_block_size int, mirror tinyint NOT NULL);
CREATE TABLE dbo.backupset (backup_set_id int IDENTITY(1,1) PRIMARY KEY, backup_set_uuid uniqueidentifier NOT NULL, media_set_id int NOT NULL, first_family_number tinyint, first_media_number smallint, last_family_number tinyint, last_media_number smallint, catalog_family_number tinyint, catalog_media_number smallint, position int, expiration_date datetime, software_vendor_id int, name nvarchar(128), description nvarchar(255), user_name nvarchar(128), software_major_version tinyint, software_minor_version tinyint, software_build_version smallint, time_zone smallint, mtf_minor_version tinyint, first_lsn numeric(25,0), last_lsn numeric(25,0), checkpoint_lsn numeric(25,0), database_backup_lsn numeric(25,0), database_creation_date datetime, backup_start_date datetime, backup_finish_date datetime, type char(1), sort_order smallint, code_page smallint, compatibility_level tinyint, database_version int, backup_size numeric(20,0), database_name nvarchar(128), server_name nvarchar(128), machine_name nvarchar(128), flags int, unicode_locale int, unicode_compare_style int, collation_name nvarchar(128), is_password_protected bit, recovery_model nvarchar(60), has_bulk_logged_data bit, is_snapshot bit, is_readonly bit, is_single_user bit, has_backup_checksums bit, is_damaged bit, begins_log_chain bit, has_incomplete_metadata bit, is_force_offline bit, is_copy_only bit, first_recovery_fork_guid uniqueidentifier, last_recovery_fork_guid uniqueidentifier, fork_point_lsn numeric(25,0), database_guid uniqueidentifier, family_guid uniqueidentifier, differential_base_lsn numeric(25,0), differential_base_guid uniqueidentifier, backup_channel_type tinyint, backup_channel_type_desc nvarchar(60), compressed_backup_size numeric(20,0), key_algorithm nvarchar(32), encryptor_thumbprint varbinary(20), encryptor_type nvarchar(32));
CREATE TABLE dbo.backupfile (backup_set_id int NOT NULL, first_family_number tinyint, first_media_number smallint, filegroup_name nvarchar(128), page_size int, file_number numeric(10,0) NOT NULL, backed_up_page_count numeric(10,0), file_type char(1), source_file_block_size numeric(10,0), file_size numeric(20,0), logical_name nvarchar(128), physical_name nvarchar(260), state tinyint, state_desc nvarchar(60), create_lsn numeric(25,0), drop_lsn numeric(25,0), file_guid uniqueidentifier, read_only_lsn numeric(25,0), read_write_lsn numeric(25,0), differential_base_lsn numeric(25,0), differential_base_guid uniqueidentifier, backup_size numeric(20,0), filegroup_guid uniqueidentifier, is_readonly bit, is_present bit);
CREATE TABLE dbo.backupfilegroup (backup_set_id int NOT NULL, name nvarchar(128), filegroup_id int, filegroup_guid uniqueidentifier, type char(2), type_desc nvarchar(60), is_default bit, is_readonly bit, log_filegroup_guid uniqueidentifier);
CREATE TABLE dbo.restorehistory (restore_history_id int IDENTITY(1,1) PRIMARY KEY, restore_date datetime, destination_database_name nvarchar(128), user_name nvarchar(128), backup_set_id int NOT NULL, restore_type char(1), [replace] bit, recovery bit, restart bit, stop_at datetime, device_count tinyint, stop_at_mark_name nvarchar(128), stop_before bit);
CREATE TABLE dbo.restorefile (restore_history_id int NOT NULL, file_number numeric(10,0), destination_phys_name nvarchar(260));
CREATE TABLE dbo.restorefilegroup (restore_history_id int NOT NULL, filegroup_name nvarchar(128));
";

/// What one backup leaves in the history.
pub(crate) struct BackupRecord {
    pub db: String,
    /// `D` database, `I` differential, `L` log
    pub kind: char,
    pub name: String,
    pub description: String,
    pub path: String,
    pub size: u64,
    pub start: chrono::NaiveDateTime,
    pub finish: chrono::NaiveDateTime,
    pub first_lsn: u64,
    pub last_lsn: u64,
    pub diff_base: Option<u64>,
    pub copy_only: bool,
    pub recovery_full: bool,
    pub collation: String,
    pub created: chrono::NaiveDateTime,
}

/// What one restore leaves in the history.
pub(crate) struct RestoreRecord {
    pub db: String,
    pub kind: char,
    pub replace: bool,
    pub recovery: bool,
    pub path: String,
}

fn q(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

fn ts(t: chrono::NaiveDateTime) -> String {
    format!("'{}'", t.format("%Y-%m-%dT%H:%M:%S%.3f"))
}

impl<'a> Engine<'a> {
    /// Run `sql` as the server, in `msdb`, in a scope of its own (its variables are not the caller's).
    fn run_as_server_in_msdb(&mut self, sql: &str) -> Result<()> {
        let stmts = super::cached_parse(sql)?;
        let saved_db = std::mem::replace(&mut self.ses.db, "msdb".to_string());
        let saved_ctx = std::mem::replace(&mut self.ses.system_ctx, true);
        self.ses.frames.push(Frame::default());
        let r = self.exec_stmts(&stmts);
        self.ses.frames.pop();
        self.ses.system_ctx = saved_ctx;
        self.ses.db = saved_db;
        match r {
            Ok(()) | Err(Interrupt::Return) => Ok(()),
            Err(Interrupt::Error(e)) => Err(e),
            Err(_) => Ok(()),
        }
    }

    /// Make the history tables the first time one is needed.
    fn ensure_history_tables(&mut self) -> Result<()> {
        let have = self.srv.databases.get("msdb").map(|d| d.objects.contains_key("dbo.backupset")).unwrap_or(false);
        if have {
            return Ok(());
        }
        self.run_as_server_in_msdb(HISTORY_DDL)
    }

    /// Record a backup that succeeded.
    pub(crate) fn record_backup(&mut self, r: &BackupRecord) {
        let _ = self.try_record_backup(r);
    }

    fn try_record_backup(&mut self, r: &BackupRecord) -> Result<()> {
        self.ensure_history_tables()?;
        let user = self.eff_login();
        let server = self.srv.server_name.clone();
        let machine = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| server.clone());
        let size = r.size;
        let diff_lsn = r.diff_base.map(|d| d.to_string()).unwrap_or_else(|| "NULL".into());
        let recovery = if r.recovery_full { "FULL" } else { "SIMPLE" };
        let begins_chain = (r.kind == 'D' && !r.copy_only && r.recovery_full) as i32;
        let name = if r.name.is_empty() { "NULL".to_string() } else { q(&r.name) };
        let desc = if r.description.is_empty() { "NULL".to_string() } else { q(&r.description) };
        let sql = format!(
            "DECLARE @m int, @b int;
             INSERT dbo.backupmediaset (media_uuid, media_family_count, name, description, software_name, software_vendor_id, MTF_major_version, mirror_count, is_password_protected, is_compressed)
               VALUES (NEWID(), 1, NULL, NULL, N'Microsoft SQL Server', 4608, 1, 1, 0, 0);
             SET @m = SCOPE_IDENTITY();
             INSERT dbo.backupmediafamily (media_set_id, family_sequence_number, media_family_id, media_count, logical_device_name, physical_device_name, device_type, physical_block_size, mirror)
               VALUES (@m, 1, NEWID(), 1, NULL, {path}, 2, 512, 0);
             INSERT dbo.backupset (backup_set_uuid, media_set_id, first_family_number, first_media_number, last_family_number, last_media_number, catalog_family_number, catalog_media_number, position, software_vendor_id, name, description, user_name, software_major_version, software_minor_version, software_build_version, time_zone, mtf_minor_version, first_lsn, last_lsn, checkpoint_lsn, database_backup_lsn, database_creation_date, backup_start_date, backup_finish_date, type, sort_order, code_page, compatibility_level, database_version, backup_size, database_name, server_name, machine_name, flags, unicode_locale, unicode_compare_style, collation_name, is_password_protected, recovery_model, has_bulk_logged_data, is_snapshot, is_readonly, is_single_user, has_backup_checksums, is_damaged, begins_log_chain, has_incomplete_metadata, is_force_offline, is_copy_only, differential_base_lsn, backup_channel_type, backup_channel_type_desc, compressed_backup_size)
               VALUES (NEWID(), @m, 1, 1, 1, 1, 1, 1, 1, 4608, {name}, {desc}, {user}, 16, 0, 1000, -1, 0, {first}, {last}, {last}, {first}, {created}, {start}, {finish}, '{kind}', 52, 1252, 160, 957, {size}, {db}, {server}, {machine}, 512, 1033, 0, {coll}, 0, '{recovery}', 0, 0, 0, 0, 1, 0, {begins}, 0, 0, {copy}, {diff}, 1, N'Disk', {size});
             SET @b = SCOPE_IDENTITY();
             INSERT dbo.backupfile (backup_set_id, first_family_number, first_media_number, filegroup_name, page_size, file_number, backed_up_page_count, file_type, source_file_block_size, file_size, logical_name, physical_name, state, state_desc, backup_size, is_readonly, is_present)
               VALUES (@b, 1, 1, N'PRIMARY', 8192, 1, {pages}, 'D', 512, {size}, {db}, {dbmdf}, 0, N'ONLINE', {size}, 0, 1),
                      (@b, 1, 1, NULL, 512, 2, 0, 'L', 512, 0, {dblog}, {dbldf}, 0, N'ONLINE', 0, 0, 1);",
            path = q(&r.path),
            name = name,
            desc = desc,
            user = q(&user),
            first = r.first_lsn,
            last = r.last_lsn,
            created = ts(r.created),
            start = ts(r.start),
            finish = ts(r.finish),
            kind = r.kind,
            size = size,
            db = q(&r.db),
            server = q(&server),
            machine = q(&machine),
            coll = q(&r.collation),
            recovery = recovery,
            begins = begins_chain,
            copy = r.copy_only as i32,
            diff = diff_lsn,
            pages = size / 8192,
            dbmdf = q(&format!("{}.mdf", r.db)),
            dblog = q(&format!("{}_log", r.db)),
            dbldf = q(&format!("{}_log.ldf", r.db)),
        );
        self.run_as_server_in_msdb(&sql)
    }

    /// Record a restore that succeeded, against the newest backup set that was taken from the file (when the history knows it).
    pub(crate) fn record_restore(&mut self, r: &RestoreRecord) {
        let _ = self.try_record_restore(r);
    }

    fn try_record_restore(&mut self, r: &RestoreRecord) -> Result<()> {
        let have = self.srv.databases.get("msdb").map(|d| d.objects.contains_key("dbo.restorehistory")).unwrap_or(false);
        if !have {
            self.ensure_history_tables()?;
        }
        let user = self.eff_login();
        let sql = format!(
            "DECLARE @h int, @s int;
             SET @s = (SELECT TOP 1 bs.backup_set_id FROM dbo.backupset bs JOIN dbo.backupmediafamily mf ON mf.media_set_id = bs.media_set_id WHERE mf.physical_device_name = {path} ORDER BY bs.backup_set_id DESC);
             INSERT dbo.restorehistory (restore_date, destination_database_name, user_name, backup_set_id, restore_type, [replace], recovery, restart, device_count)
               VALUES (GETDATE(), {db}, {user}, ISNULL(@s, 0), '{kind}', {replace}, {recovery}, 0, 1);
             SET @h = SCOPE_IDENTITY();
             INSERT dbo.restorefile (restore_history_id, file_number, destination_phys_name) VALUES (@h, 1, {mdf});",
            path = q(&r.path),
            db = q(&r.db),
            user = q(&user),
            kind = r.kind,
            replace = r.replace as i32,
            recovery = r.recovery as i32,
            mdf = q(&format!("{}.mdf", r.db)),
        );
        self.run_as_server_in_msdb(&sql)
    }

    /// `sp_delete_database_backuphistory @database_name`: the history of the database (its restores too) is removed.
    pub(crate) fn delete_database_backup_history(&mut self, db: &str) -> Result<()> {
        self.delete_history_where(&format!("bs.database_name = {}", q(db)), Some(format!("destination_database_name = {}", q(db))))
    }

    /// `sp_delete_backuphistory @oldest_date`: the history older than the date.
    pub(crate) fn delete_backup_history_before(&mut self, date: chrono::NaiveDateTime) -> Result<()> {
        self.delete_history_where(&format!("bs.backup_finish_date < {}", ts(date)), Some(format!("restore_date < {}", ts(date))))
    }

    fn delete_history_where(&mut self, backup_cond: &str, restore_cond: Option<String>) -> Result<()> {
        let have = self.srv.databases.get("msdb").map(|d| d.objects.contains_key("dbo.backupset")).unwrap_or(false);
        if !have {
            return Ok(());
        }
        let restore_cond = restore_cond.unwrap_or_else(|| "1 = 0".into());
        let sql = format!(
            "DELETE FROM dbo.restorefilegroup WHERE restore_history_id IN (SELECT restore_history_id FROM dbo.restorehistory WHERE {rc});
             DELETE FROM dbo.restorefile WHERE restore_history_id IN (SELECT restore_history_id FROM dbo.restorehistory WHERE {rc});
             DELETE FROM dbo.restorehistory WHERE {rc};
             DELETE FROM dbo.backupfile WHERE backup_set_id IN (SELECT bs.backup_set_id FROM dbo.backupset bs WHERE {bc});
             DELETE FROM dbo.backupfilegroup WHERE backup_set_id IN (SELECT bs.backup_set_id FROM dbo.backupset bs WHERE {bc});
             DELETE FROM dbo.backupmediafamily WHERE media_set_id IN (SELECT bs.media_set_id FROM dbo.backupset bs WHERE {bc});
             DELETE FROM dbo.backupmediaset WHERE media_set_id IN (SELECT bs.media_set_id FROM dbo.backupset bs WHERE {bc});
             DELETE FROM dbo.backupset WHERE backup_set_id IN (SELECT bs.backup_set_id FROM dbo.backupset bs WHERE {bc});",
            rc = restore_cond,
            bc = backup_cond
        );
        self.run_as_server_in_msdb(&sql)
    }
}
