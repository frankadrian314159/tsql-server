//! Catalog views and functions that GUI clients (DBeaver, SSMS, Azure Data Studio) query to build their navigators.
mod common;
use common::*;

fn setup(c: &mut tsql_server::Connection) {
    script(
        c,
        "CREATE TABLE dbo.p (id INT IDENTITY PRIMARY KEY, name VARCHAR(10) CONSTRAINT df_name DEFAULT 'x', code INT UNIQUE);
         CREATE TABLE dbo.k (id INT PRIMARY KEY, pid INT CONSTRAINT fk_k FOREIGN KEY REFERENCES dbo.p (id), pid2 INT CONSTRAINT fk_k2 REFERENCES dbo.p (id));
         CREATE INDEX ix_k ON dbo.k (pid, pid2 DESC) INCLUDE (id);
         CREATE VIEW dbo.v AS SELECT id FROM dbo.p;
         CREATE SYNONYM dbo.sn FOR dbo.p;
         INSERT dbo.p (name, code) VALUES ('a', 1), ('b', 2), ('c', 3);",
    );
}

#[test]
fn extended_properties_exists_and_is_empty() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM master.sys.extended_properties WHERE class = 3"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.extended_properties ep JOIN sys.schemas s ON ep.major_id = s.schema_id WHERE ep.name = 'MS_Description'"), "0");
}

#[test]
fn foreign_keys_have_their_own_ids_and_columns() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(scalar(&mut c, "SELECT COUNT(DISTINCT object_id) FROM sys.foreign_keys"), "2");
    let r = rows(&mut c, "SELECT fk.name, fkc.constraint_column_id, fkc.parent_column_id, fkc.referenced_column_id FROM sys.foreign_keys fk JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id ORDER BY fk.name");
    assert_eq!(r, [["fk_k", "1", "2", "1"], ["fk_k2", "1", "3", "1"]]);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.foreign_key_columns fkc JOIN sys.tables t ON t.object_id = fkc.parent_object_id AND t.name = 'k' JOIN sys.tables r ON r.object_id = fkc.referenced_object_id AND r.name = 'p'"), "2");
}

#[test]
fn default_constraints_join_to_their_columns() {
    let mut c = conn();
    setup(&mut c);
    let r = rows(&mut c, "SELECT dc.name, c.name, dc.definition FROM sys.default_constraints dc JOIN sys.columns c ON c.object_id = dc.parent_object_id AND c.column_id = dc.parent_column_id");
    assert_eq!(r, [["df_name", "name", "('x')"]]);
    assert_eq!(scalar(&mut c, "SELECT dc.name FROM sys.columns c JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id WHERE c.name = 'name'"), "df_name");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.columns c WHERE c.name = 'code' AND c.default_object_id = 0"), "1");
}

#[test]
fn synonyms_are_listed() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(rows(&mut c, "SELECT name, base_object_name, type FROM sys.synonyms"), [["sn", "[dbo].[p]", "SN"]]);
}

#[test]
fn indexes_report_type_uniqueness_and_key_ordinals() {
    let mut c = conn();
    setup(&mut c);
    let r = rows(&mut c, "SELECT i.name, i.type, i.type_desc, i.is_unique, i.is_primary_key, i.is_unique_constraint FROM sys.indexes i WHERE i.object_id = OBJECT_ID('dbo.p') ORDER BY i.index_id");
    assert_eq!(r.len(), 2);
    assert_eq!((r[0][1].as_str(), r[0][2].as_str(), r[0][3].as_str(), r[0][4].as_str(), r[0][5].as_str()), ("1", "CLUSTERED", "1", "1", "0"));
    assert_eq!((r[1][1].as_str(), r[1][2].as_str(), r[1][3].as_str(), r[1][4].as_str(), r[1][5].as_str()), ("2", "NONCLUSTERED", "1", "0", "1"));
    let r = rows(&mut c, "SELECT ic.column_id, ic.key_ordinal, ic.is_descending_key, ic.is_included_column FROM sys.indexes i JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id WHERE i.name = 'ix_k' ORDER BY ic.is_included_column, ic.index_column_id");
    assert_eq!(r, [["2", "1", "0", "0"], ["3", "2", "1", "0"], ["1", "0", "0", "1"]]);
}

#[test]
fn objects_types_partitions_and_files() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.all_objects WHERE is_ms_shipped = 1"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.tables WHERE is_ms_shipped = 0"), "2");
    let r = rows(&mut c, "SELECT name, max_length, precision, scale, is_nullable FROM sys.types WHERE name IN ('int', 'varchar', 'decimal', 'datetime2') ORDER BY name");
    assert_eq!(r, [["datetime2", "8", "27", "7", "1"], ["decimal", "17", "18", "0", "1"], ["int", "4", "10", "0", "1"], ["varchar", "8000", "0", "0", "1"]]);
    assert_eq!(scalar(&mut c, "SELECT row_count FROM sys.dm_db_partition_stats WHERE object_id = OBJECT_ID('dbo.p')"), "3");
    assert_eq!(scalar(&mut c, "SELECT SUM(rows) FROM sys.partitions WHERE object_id = OBJECT_ID('dbo.p') AND index_id IN (0, 1)"), "3");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.database_files WHERE type_desc = 'ROWS'"), "1");
}

#[test]
fn object_definition_returns_the_text_of_modules() {
    let mut c = conn();
    setup(&mut c);
    script(&mut c, "CREATE PROCEDURE dbo.pr AS SELECT 1;\nGO\nCREATE FUNCTION dbo.fn() RETURNS INT AS BEGIN RETURN 1; END;\nGO\nCREATE TRIGGER dbo.tr ON dbo.p AFTER INSERT AS SELECT 1;");
    for (obj, needle) in [("dbo.v", "CREATE VIEW dbo.v"), ("dbo.pr", "CREATE PROCEDURE dbo.pr"), ("dbo.fn", "CREATE FUNCTION dbo.fn"), ("dbo.tr", "CREATE TRIGGER dbo.tr")] {
        let text = scalar(&mut c, &format!("SELECT OBJECT_DEFINITION(OBJECT_ID('{}'))", obj));
        assert!(text.contains(needle), "{}: {}", obj, text);
    }
    assert_eq!(scalar(&mut c, "SELECT OBJECT_DEFINITION(OBJECT_ID('dbo.p'))"), "NULL");
    assert_eq!(scalar(&mut c, "SELECT OBJECT_DEFINITION(NULL)"), "NULL");
}

#[test]
fn sp_help_and_sp_columns_accept_schema_qualified_names() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(rows(&mut c, "EXEC sp_columns 'dbo.p'").len(), 3);
    assert_eq!(rows(&mut c, "EXEC sp_columns '[dbo].[p]'").len(), 3);
    assert_eq!(rows(&mut c, "EXEC sp_help 'dbo.p'").len(), 3);
}

#[test]
fn computed_columns_are_listed() {
    let mut c = conn();
    setup(&mut c);
    script(&mut c, "CREATE TABLE dbo.cc (a INT, b INT, s AS a + b)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM master.sys.computed_columns WHERE object_id = OBJECT_ID('dbo.cc')"), "1");
    assert_eq!(scalar(&mut c, "SELECT name FROM sys.computed_columns WHERE object_id = OBJECT_ID('dbo.cc')"), "s");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.computed_columns WHERE object_id = OBJECT_ID('dbo.p')"), "0");
}

#[test]
fn the_host_and_the_instance_can_be_asked_who_they_are() {
    // (SSMS reads these when it connects)
    let mut c = conn();
    let host = rows(&mut c, "SELECT host_platform, host_release, os_language_version FROM sys.dm_os_host_info");
    assert_eq!(host.len(), 1);
    assert!(matches!(host[0][0].as_str(), "Windows" | "Linux"), "{host:?}");
    assert_eq!(host[0][2], "1033");
    let info = rows(&mut c, "SELECT cpu_count, scheduler_count, affinity_type_desc FROM sys.dm_os_sys_info");
    assert_eq!(info.len(), 1);
    assert_eq!(info[0][2], "AUTO");
    assert_eq!(info[0][0], info[0][1]);
}

#[test]
fn connectionproperty_of_an_embedded_connection() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT CAST(CONNECTIONPROPERTY('net_transport') AS NVARCHAR(40))"), "Shared memory");
    assert_eq!(scalar(&mut c, "SELECT CAST(CONNECTIONPROPERTY('protocol_type') AS NVARCHAR(40))"), "TSQL");
    assert_eq!(scalar(&mut c, "SELECT CAST(CONNECTIONPROPERTY('client_net_address') AS NVARCHAR(40))"), "<local machine>");
    assert_eq!(scalar(&mut c, "SELECT ISNULL(CAST(CONNECTIONPROPERTY('local_tcp_port') AS INT), -1)"), "-1");
    assert_eq!(scalar(&mut c, "SELECT ISNULL(CAST(CONNECTIONPROPERTY('nonsense') AS NVARCHAR(40)), 'none')"), "none");
}

#[test]
fn the_statements_ssms_runs_when_it_connects_all_work() {
    let mut c = conn();
    for sql in [
        "SELECT SERVERPROPERTY('EngineEdition'), SERVERPROPERTY('ProductVersion'), SERVERPROPERTY('Edition'), SERVERPROPERTY('IsClustered'), SERVERPROPERTY('ServerName'), SERVERPROPERTY('IsHadrEnabled'), SERVERPROPERTY('Collation')",
        "SELECT @@VERSION, @@SERVERNAME, @@SERVICENAME, @@LANGUAGE, @@LANGID, @@MAX_CONNECTIONS, @@TEXTSIZE, @@DATEFIRST, @@REMSERVER, @@SPID",
        "SELECT SUSER_SNAME(), SUSER_ID(), SUSER_SID(), ORIGINAL_LOGIN(), IS_SRVROLEMEMBER('sysadmin'), HAS_PERMS_BY_NAME(null, null, 'VIEW SERVER STATE')",
        "SELECT SESSIONPROPERTY('ANSI_NULLS'), CONNECTIONPROPERTY('net_transport'), DATABASEPROPERTY('master', 'Collation'), DATABASEPROPERTYEX('master', 'Status')",
        "SELECT name, database_id, owner_sid, collation_name, compatibility_level, user_access_desc, is_read_only, state_desc, recovery_model_desc, snapshot_isolation_state, is_read_committed_snapshot_on, is_broker_enabled, containment, is_auto_close_on, page_verify_option_desc, create_date FROM sys.databases",
        "SELECT name, principal_id, sid, type, type_desc, is_disabled, default_database_name, default_language_name, is_fixed_role FROM sys.server_principals",
        "SELECT name, is_policy_checked, is_expiration_checked, password_hash FROM sys.sql_logins",
        "SELECT role_principal_id, member_principal_id FROM sys.server_role_members",
        "SELECT name, value, value_in_use FROM sys.configurations",
        "SELECT session_id, net_transport, auth_scheme FROM sys.dm_exec_connections",
        "SELECT database_id, name, physical_name, type_desc FROM sys.master_files",
        "SELECT name, product, is_linked FROM sys.servers",
        "SELECT name, protocol_desc, type_desc, state_desc FROM sys.endpoints",
        "SELECT * FROM sys.server_permissions", "SELECT * FROM sys.assemblies", "SELECT * FROM sys.availability_groups", "SELECT * FROM sys.dm_hadr_availability_replica_states",
        "SELECT * FROM sys.resource_governor_configuration", "SELECT * FROM sys.traces", "SELECT * FROM sys.credentials", "SELECT * FROM sys.linked_logins", "SELECT * FROM sys.backup_devices",
        "SELECT * FROM sys.change_tracking_databases", "SELECT * FROM sys.sysdatabases", "SELECT * FROM sys.dm_os_sys_memory", "SELECT * FROM sys.dm_os_process_memory", "SELECT * FROM sys.dm_server_services",
        "SELECT * FROM sys.dm_os_performance_counters", "SELECT * FROM sys.dm_os_wait_stats", "SELECT * FROM sys.dm_os_host_info",
        "EXEC sp_server_info", "EXEC xp_msver", "EXEC sp_databases",
        "SELECT HAS_DBACCESS('master'), FILE_ID('master'), FILE_NAME(1), FILEGROUP_ID('PRIMARY'), FILEGROUP_NAME(1), FILEGROUPPROPERTY('PRIMARY', 'IsDefault'), FILEPROPERTY('master', 'IsPrimaryFile'), TYPEPROPERTY('int', 'Precision'), IS_ROLEMEMBER('public'), USER_ID(), DATABASE_PRINCIPAL_ID('dbo'), ORIGINAL_DB_NAME(), SESSION_CONTEXT(N'k'), CURRENT_REQUEST_ID(), OBJECTPROPERTYEX(1, 'IsTable'), INDEXPROPERTY(1, 'ix', 'IsUnique')",
        "SELECT FULLTEXTSERVICEPROPERTY('IsFullTextInstalled'), FULLTEXTSERVICEPROPERTY('ResourceUsage'), FULLTEXTCATALOGPROPERTY('c', 'ItemCount')",
    ] {
        let r = c.execute(sql);
        assert!(r.first_error().is_none(), "{sql}: {:?}", r.first_error());
    }
    // what they say
    assert_eq!(rows(&mut c, "SELECT name FROM sys.server_principals WHERE type = 'R' ORDER BY principal_id").first().unwrap(), &["sysadmin"]);
    assert_eq!(scalar(&mut c, "SELECT type_desc FROM sys.server_principals WHERE name = 'sa'"), "SQL_LOGIN");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.server_role_members m JOIN sys.server_principals r ON r.principal_id = m.role_principal_id JOIN sys.server_principals p ON p.principal_id = m.member_principal_id WHERE r.name = 'sysadmin' AND p.name = 'sa'"), "1");
    assert_eq!(scalar(&mut c, "SELECT SUSER_ID('sa') - (SELECT principal_id FROM sys.server_principals WHERE name = 'sa')"), "0");
    assert_eq!(scalar(&mut c, "SELECT user_access_desc FROM sys.databases WHERE name = 'master'"), "MULTI_USER");
    // what SSMS reads of each database: the columns line up with their names, and the views of availability and mirroring are there (empty)
    assert_eq!(rows(&mut c, "SELECT is_distributor, log_reuse_wait, log_reuse_wait_desc, containment, containment_desc, delayed_durability_desc, is_ledger_on FROM sys.databases WHERE name = 'master'"), vec![vec!["0".to_string(), "0".into(), "NOTHING".into(), "0".into(), "NONE".into(), "DISABLED".into(), "0".into()]]);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.availability_replicas"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.dm_hadr_database_replica_states"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.database_mirroring WHERE mirroring_role IS NULL AND database_id = 1"), "1");
    assert_eq!(scalar(&mut c, "SELECT HAS_DBACCESS('master')"), "1");
    assert_eq!(scalar(&mut c, "SELECT ISNULL(HAS_DBACCESS('nowhere'), -1)"), "-1");
    exec(&mut c, "CREATE TABLE ixt (a INT PRIMARY KEY, b INT, c INT)");
    exec(&mut c, "CREATE UNIQUE INDEX ix_b ON ixt (b)");
    exec(&mut c, "CREATE INDEX ix_c ON ixt (c)");
    assert_eq!(rows(&mut c, "SELECT INDEXPROPERTY(OBJECT_ID('ixt'), 'ix_b', 'IsUnique'), INDEXPROPERTY(OBJECT_ID('ixt'), 'ix_c', 'IsUnique'), INDEXPROPERTY(OBJECT_ID('ixt'), 'ix_c', 'IsClustered')"), vec![vec!["1".to_string(), "0".into(), "0".into()]]);
    assert_eq!(scalar(&mut c, "SELECT FILE_NAME(FILE_ID('master'))"), "master");
    exec(&mut c, "CREATE DATABASE snap");
    exec(&mut c, "ALTER DATABASE snap SET ALLOW_SNAPSHOT_ISOLATION ON");
    assert_eq!(scalar(&mut c, "SELECT snapshot_isolation_state_desc FROM sys.databases WHERE name = 'snap'"), "ON");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.master_files WHERE type_desc = 'LOG'"), scalar(&mut c, "SELECT COUNT(*) FROM sys.databases"));
}

#[test]
fn the_server_properties_that_ssms_reads_into_numbers_and_booleans_are_not_null() {
    // (a client that converts a NULL to a boolean or a number stops with "Object cannot be cast from DBNull to other types")
    let mut c = conn();
    for p in [
        "ProductVersion", "ProductMajorVersion", "ProductMinorVersion", "ProductBuild", "ProductLevel", "ResourceVersion", "Edition", "EditionID", "EngineEdition", "ServerName", "MachineName",
        "ComputerNamePhysicalNetBIOS", "Collation", "LCID", "SqlCharSet", "SqlSortOrder", "ComparisonStyle", "IsClustered", "IsIntegratedSecurityOnly", "IsSingleUser", "IsHadrEnabled", "IsLocalDB",
        "IsXTPSupported", "IsPolyBaseInstalled", "IsFullTextInstalled", "FilestreamConfiguredLevel", "FilestreamEffectiveLevel", "LicenseType", "BuildClrVersion", "ProcessID", "PathSeparator",
        "InstanceDefaultDataPath", "InstanceDefaultLogPath", "InstanceDefaultBackupPath", "ErrorLogFileName", "IsExternalAuthenticationOnly", "IsBigDataCluster",
    ] {
        assert_ne!(scalar(&mut c, &format!("SELECT SERVERPROPERTY('{p}')")), "NULL", "{p}");
    }
    // a default instance has no instance name
    assert_eq!(scalar(&mut c, "SELECT SERVERPROPERTY('InstanceName')"), "NULL");
    assert_eq!(scalar(&mut c, "SELECT SERVERPROPERTY('IsHadrEnabled')"), "0");
    assert_eq!(scalar(&mut c, "SELECT SERVERPROPERTY('LCID')"), "1033");
}
