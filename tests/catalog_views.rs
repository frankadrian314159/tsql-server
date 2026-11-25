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
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.all_objects WHERE is_ms_shipped = 1"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.tables WHERE is_ms_shipped = 0"), "2");
    let r = rows(&mut c, "SELECT name, max_length, precision, scale, is_nullable FROM sys.types WHERE name IN ('int', 'varchar', 'decimal', 'datetime2') ORDER BY name");
    assert_eq!(r, [["datetime2", "8", "27", "7", "1"], ["decimal", "17", "18", "0", "1"], ["int", "4", "10", "0", "1"], ["varchar", "8000", "0", "0", "1"]]);
    assert_eq!(scalar(&mut c, "SELECT row_count FROM sys.dm_db_partition_stats WHERE object_id = OBJECT_ID('dbo.p')"), "3");
    assert_eq!(scalar(&mut c, "SELECT SUM(rows) FROM sys.partitions WHERE object_id = OBJECT_ID('dbo.p') AND index_id IN (0, 1)"), "3");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.database_files WHERE type_desc = 'ROWS'"), "1");
    // the Memory Optimization Advisor's query of a file group: the files of the group by its id
    assert_eq!(
        scalar(&mut c, "SELECT CAST(ISNULL((select sum(cast(gs.size as float))*convert(float,8) from sys.database_files gs where gs.data_space_id = g.data_space_id), 0) AS float) FROM sys.filegroups AS g WHERE g.name = 'PRIMARY'"),
        "1024"
    );
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
    // (a table, so that the columns of the views are looked up: with none, a query over a view that has no rows may not look at its columns)
    exec(&mut c, "CREATE TABLE ssms_t (a INT PRIMARY KEY, b INT NULL, CONSTRAINT ck_ssms CHECK (a > 0))");
    exec(&mut c, "CREATE INDEX ix_ssms ON ssms_t (b)");
    for sql in [
        // the settings SSMS applies to each query window's connection
        "SET ROWCOUNT 0 SET TEXTSIZE 2147483647 SET NOCOUNT OFF SET CONCAT_NULL_YIELDS_NULL ON SET ARITHABORT ON SET LOCK_TIMEOUT -1 SET QUERY_GOVERNOR_COST_LIMIT 0 SET DEADLOCK_PRIORITY NORMAL SET TRANSACTION ISOLATION LEVEL READ COMMITTED SET ANSI_NULLS ON SET ANSI_NULL_DFLT_ON ON SET ANSI_PADDING ON SET ANSI_WARNINGS ON SET CURSOR_CLOSE_ON_COMMIT OFF SET IMPLICIT_TRANSACTIONS OFF SET QUOTED_IDENTIFIER ON",
        "SET DEADLOCK_PRIORITY LOW SET DEADLOCK_PRIORITY HIGH SET DEADLOCK_PRIORITY -3 SET DEADLOCK_PRIORITY NORMAL",
        // what SSMS reads to script an object and to list its database: the columns of a database and of its principals, the old system tables
        "SELECT dtb.name, dtb.catalog_collation_type, dtb.catalog_collation_type_desc, dtb.is_ledger_on, dtb.default_language_name, dtb.is_query_store_on, dtb.two_digit_year_cutoff, dtb.resource_pool_id FROM master.sys.databases AS dtb LEFT OUTER JOIN sys.master_files AS df ON df.database_id = dtb.database_id AND 1 = df.data_space_id AND 1 = df.file_id",
        "SELECT name, type_desc, create_date, is_fixed_role, authentication_type_desc, sid FROM sys.database_principals",
        "SELECT v.name, v.has_opaque_metadata, v.with_check_option FROM sys.all_views AS v LEFT OUTER JOIN sys.sql_modules AS m ON m.object_id = v.object_id LEFT OUTER JOIN sys.system_sql_modules AS sm ON sm.object_id = v.object_id",
        "SELECT name, system_type_id, is_table_type, type_table_object_id, is_memory_optimized FROM sys.table_types",
        "SELECT su.name, so.name, isr.DATA_TYPE, isr.SPECIFIC_NAME FROM sysobjects so INNER JOIN sysusers su ON so.uid = su.uid INNER JOIN INFORMATION_SCHEMA.ROUTINES isr ON so.name = isr.SPECIFIC_NAME WHERE so.type = N'AF'",
        "SELECT su.name, so.name FROM sysobjects so INNER JOIN sysusers su ON so.uid = su.uid WHERE so.type = N'AF'",
        "SELECT name FROM master.dbo.sysdatabases",
        // what SMO reads of a column to script it: the encryption keys and classifications, the identity and computed columns
        "SELECT clmns.name, clmns.system_type_id, clmns.column_encryption_key_id, ek.name, sc.label, ic.is_not_for_replication, ic.seed_value, cc.definition FROM sys.all_columns AS clmns LEFT OUTER JOIN sys.column_encryption_keys ek ON clmns.column_encryption_key_id = ek.column_encryption_key_id LEFT OUTER JOIN sys.sensitivity_classifications sc ON sc.major_id = clmns.object_id AND sc.minor_id = clmns.column_id LEFT OUTER JOIN sys.identity_columns ic ON ic.object_id = clmns.object_id AND ic.column_id = clmns.column_id LEFT OUTER JOIN sys.computed_columns cc ON cc.object_id = clmns.object_id AND cc.column_id = clmns.column_id",
        "SELECT usrt.name, usrt.schema_id, usrt.is_table_type, usrt.is_assembly_type, usrt.collation_name, usrt.default_object_id FROM sys.types usrt WHERE usrt.is_table_type = 0",
        "SELECT name FROM sys.column_master_keys UNION ALL SELECT CAST(column_encryption_key_id AS NVARCHAR(20)) FROM sys.column_encryption_key_values",
        // what SSMS reads of a table for its properties: change tracking, external and file tables, the period, the ledger
        "SELECT t.object_id, c.is_track_columns_updated_on, c.begin_version FROM sys.tables t INNER JOIN sys.change_tracking_tables AS c ON c.object_id = t.object_id",
        "SELECT et.object_id, ds.name, ff.name FROM sys.external_tables et LEFT JOIN sys.external_data_sources ds ON ds.data_source_id = et.data_source_id LEFT JOIN sys.external_file_formats ff ON ff.file_format_id = et.file_format_id",
        "SELECT t.object_id FROM sys.tables t INNER JOIN sys.filetables f ON t.object_id = f.object_id",
        "SELECT t.object_id, t.ledger_view_id, t.history_retention_period, t.history_retention_period_unit_desc, t.Filestream_data_space_id, p.start_column_id, c.is_data_deletion_filter_column, c.ledger_view_column_type FROM sys.tables t LEFT JOIN sys.periods p ON p.object_id = t.object_id JOIN sys.columns c ON c.object_id = t.object_id",
        // the index and login properties SSMS reads when it scripts a table or shows the logins
        "SELECT it.parent_id, it.object_id FROM sys.internal_tables it WHERE it.name LIKE 'extended[_]index[_]%'",
        "SELECT object_id, index_id, MAX(data_compression), MAX(CASE WHEN xml_compression = 1 THEN 1 ELSE 0 END), MAX(data_compression_desc) FROM sys.partitions GROUP BY object_id, index_id",
        "SELECT i.name, CAST(i.compression_delay AS INT), i.is_ignored_in_optimization, hi.bucket_count, op.state, ip.path FROM sys.indexes i LEFT JOIN sys.hash_indexes hi ON hi.object_id = i.object_id AND hi.index_id = i.index_id LEFT JOIN sys.index_resumable_operations op ON op.index_id = i.index_id AND op.object_id = i.object_id LEFT JOIN sys.selective_xml_index_paths ip ON ip.object_id = i.object_id LEFT JOIN sys.filetable_system_defined_objects ft ON ft.object_id = i.object_id",
        "SELECT log.name, l.alias, sys.fn_varbintohexsubstring(0, log.sid, 1, 0), ak.name, cert.name, c.name FROM sys.server_principals AS log LEFT OUTER JOIN sys.syslanguages AS l ON l.name = log.default_language_name LEFT OUTER JOIN master.sys.asymmetric_keys AS ak ON ak.sid = log.sid LEFT OUTER JOIN master.sys.certificates AS cert ON cert.sid = log.sid LEFT OUTER JOIN sys.credentials AS c ON log.credential_id = c.credential_id",
        "SELECT tr.name, tr.object_id, tr.is_ms_shipped, tr.type, ssmod.definition FROM master.sys.server_triggers AS tr LEFT OUTER JOIN sys.server_sql_modules AS ssmod ON ssmod.object_id = tr.object_id WHERE (tr.parent_class = 100) ORDER BY 1",
        "SELECT name FROM sys.partition_schemes UNION ALL SELECT name FROM sys.partition_functions UNION ALL SELECT name FROM sys.certificates",
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
    // the table list of SSMS: sys.tables has the columns it filters on, and the indexes are joined to the data space
    exec(&mut c, "CREATE TABLE tl (a INT PRIMARY KEY)");
    assert_eq!(rows(&mut c, "SELECT t.name, t.is_memory_optimized, t.temporal_type, t.is_external, t.is_filetable, t.ledger_type, t.is_node, t.is_edge, ds.name FROM sys.tables t JOIN sys.indexes i ON i.object_id = t.object_id LEFT OUTER JOIN sys.data_spaces ds ON ds.data_space_id = i.data_space_id WHERE t.name = 'tl'"), vec![vec!["tl".to_string(), "0".into(), "0".into(), "0".into(), "0".into(), "0".into(), "0".into(), "0".into(), "PRIMARY".into()]]);
    assert_eq!(rows(&mut c, "SELECT (SELECT COUNT(*) FROM sys.all_columns WHERE object_id = OBJECT_ID('tl') AND is_sparse = 1), (SELECT COUNT(*) FROM sys.indexes WHERE object_id = OBJECT_ID('tl') AND is_hypothetical = 0 AND allow_row_locks = 1), (SELECT generated_always_type_desc FROM sys.columns WHERE object_id = OBJECT_ID('tl') AND name = 'a')"), vec![vec!["0".to_string(), "1".into(), "NOT_APPLICABLE".into()]]);
    // what SSMS asks when a table is expanded: its constraints, triggers and columns, with the columns it selects and filters on
    exec(&mut c, "CREATE TABLE tl2 (id INT PRIMARY KEY, a INT NOT NULL CONSTRAINT df_tl2_a DEFAULT (1) CONSTRAINT ck_tl2_a CHECK (a > 0), p INT NULL CONSTRAINT fk_tl2_p REFERENCES tl (a) ON DELETE CASCADE, u INT NULL CONSTRAINT uq_tl2_u UNIQUE)");
    exec(&mut c, "CREATE TRIGGER tr_tl2 ON tl2 AFTER INSERT AS BEGIN SET NOCOUNT ON END");
    assert_eq!(rows(&mut c, "SELECT (SELECT COUNT(*) FROM sys.foreign_keys f WHERE f.parent_object_id = OBJECT_ID('tl2') AND f.is_disabled = 0 AND f.is_not_trusted = 0 AND f.delete_referential_action = 1 AND f.key_index_id = 1 AND f.create_date IS NOT NULL), (SELECT COUNT(*) FROM sys.check_constraints k WHERE k.parent_object_id = OBJECT_ID('tl2') AND k.is_system_named = 0 AND k.create_date IS NOT NULL), (SELECT COUNT(*) FROM sys.default_constraints k WHERE k.parent_object_id = OBJECT_ID('tl2') AND k.create_date IS NOT NULL), (SELECT COUNT(*) FROM sys.key_constraints k WHERE k.parent_object_id = OBJECT_ID('tl2') AND k.create_date IS NOT NULL AND k.unique_index_id >= 1), (SELECT COUNT(*) FROM sys.triggers t JOIN sys.sql_modules m ON m.object_id = t.object_id WHERE t.parent_id = OBJECT_ID('tl2') AND t.create_date IS NOT NULL AND m.uses_ansi_nulls = 1), (SELECT COUNT(*) FROM sys.system_sql_modules), (SELECT COUNT(*) FROM sys.all_columns WHERE object_id = OBJECT_ID('tl2') AND is_dropped_ledger_column = 1), (SELECT value_in_use FROM sys.configurations WHERE configuration_id = 16384)"), vec![vec!["1".to_string(), "1".into(), "1".into(), "2".into(), "1".into(), "0".into(), "0".into(), "0".into()]]);
    // the server information SSMS reads before it shows the rows of a table: registry values (NULL here), the cluster and statistics views
    assert_eq!(rows(&mut c, "DECLARE @n INT; EXEC master.dbo.xp_instance_regread N'HKEY_LOCAL_MACHINE', N'SOFTWARE\\x', N'NumErrorLogs', @n OUTPUT; SELECT ISNULL(@n, -1), @@MAX_PRECISION, ISNULL(suser_sname(sid_binary(N'nobody')), N'none'), (SELECT COUNT(*) FROM sys.dm_hadr_cluster), (SELECT COUNT(*) FROM sys.stats), (SELECT COUNT(*) FROM sys.dm_hadr_cluster_members)"), vec![vec!["-1".to_string(), "38".into(), "none".into(), "0".into(), "5".into(), "0".into()]]);
    // View Dependencies: the references of views, procedures, functions and triggers, and the collation DATABASE_DEFAULT of its temporary tables
    exec(&mut c, "CREATE TABLE dep_a (x INT)");
    exec(&mut c, "CREATE VIEW dep_v AS SELECT x FROM dep_a");
    exec(&mut c, "CREATE PROCEDURE dep_p AS SELECT * FROM dep_v");
    assert_eq!(rows(&mut c, "SELECT OBJECT_NAME(referencing_id), referenced_entity_name, referenced_schema_name FROM sys.sql_expression_dependencies WHERE referencing_id IN (OBJECT_ID('dep_v'), OBJECT_ID('dep_p')) ORDER BY 1"), vec![vec!["dep_p".to_string(), "dep_v".into(), "dbo".into()], vec!["dep_v".to_string(), "dep_a".into(), "dbo".into()]]);
    assert_eq!(rows(&mut c, "SELECT referencing_entity_name, referencing_schema_name FROM sys.dm_sql_referencing_entities('dbo.dep_a', 'OBJECT')"), vec![vec!["dep_v".to_string(), "dbo".into()]]);
    assert_eq!(rows(&mut c, "SELECT referenced_entity_name FROM sys.dm_sql_referenced_entities('dbo.dep_p', 'OBJECT')"), vec![vec!["dep_v".to_string()]]);
    exec(&mut c, "CREATE TABLE #t1 (object_name sysname collate database_default NULL, o2 VARCHAR(5) COLLATE catalog_default)");
    exec(&mut c, "INSERT #t1 VALUES (N'x', 'y')");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM #t1 WHERE object_name = N'X' COLLATE DATABASE_DEFAULT"), "1");
    assert_eq!(rows(&mut c, "SELECT name, schema_id, is_schema_bound FROM sys.security_policies"), Vec::<Vec<String>>::new());
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

#[test]
fn the_policy_store_is_there_with_a_new_servers_configuration() {
    // what SSMS reads when Policy Management is opened
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT CAST((SELECT current_value FROM msdb.dbo.syspolicy_configuration WHERE name = 'HistoryRetentionInDays') AS int)"), "90");
    assert_eq!(scalar(&mut c, "SELECT CAST((SELECT current_value FROM msdb.dbo.syspolicy_configuration WHERE name = 'Enabled') AS bit)"), "0");
}

#[test]
fn the_views_node_of_ssms_can_list_views() {
    // SSMS reads the ledger columns of sys.all_views; a missing one fails the query and the node shows nothing
    let mut c = conn();
    exec(&mut c, "CREATE VIEW dbo.v1 AS SELECT 1 AS a");
    assert_eq!(scalar(&mut c, "SELECT v.name FROM sys.all_views AS v WHERE v.type = 'V' AND CAST(ISNULL(v.ledger_view_type, 0) AS tinyint) = 0 AND v.is_ms_shipped = 0"), "v1");
}

#[test]
fn the_columns_of_a_view_are_listed_with_their_types() {
    // SSMS's Views > Columns node
    let mut c = conn();
    exec(&mut c, "CREATE TABLE dbo.t (a int NOT NULL, b nvarchar(20))");
    exec(&mut c, "CREATE VIEW dbo.v AS SELECT a, b, a + 1 AS c FROM dbo.t");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.all_columns AS c JOIN sys.all_views AS v ON c.object_id = v.object_id WHERE v.name = 'v'"), "3");
    assert_eq!(scalar(&mut c, "SELECT c.name + ':' + TYPE_NAME(c.system_type_id) FROM sys.columns AS c JOIN sys.views AS v ON c.object_id = v.object_id WHERE v.name = 'v' AND c.column_id = 2"), "b:nvarchar");
    // an altered view is listed with its new columns
    exec(&mut c, "ALTER VIEW dbo.v AS SELECT a FROM dbo.t");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.columns AS c JOIN sys.views AS v ON c.object_id = v.object_id WHERE v.name = 'v'"), "1");
}

#[test]
fn the_folders_of_ssms_under_programmability_and_synonyms_list_their_objects() {
    // the columns these queries name that were missing made the folders empty
    let mut c = conn();
    exec(&mut c, "CREATE TABLE dbo.t (a int)");
    exec(&mut c, "CREATE PROCEDURE dbo.p AS SELECT 1");
    exec(&mut c, "CREATE SYNONYM dbo.sy FOR dbo.t");
    exec(&mut c, "CREATE VIEW dbo.v AS SELECT a FROM dbo.t");
    assert_eq!(scalar(&mut c, "SELECT sp.name FROM sys.all_objects AS sp LEFT JOIN sys.all_sql_modules AS m ON m.object_id = sp.object_id WHERE sp.type = 'P' AND sp.is_ms_shipped = 0"), "p");
    assert_eq!(scalar(&mut c, "SELECT CAST(s.create_date AS date) FROM sys.synonyms AS s WHERE s.name = 'sy'").len() > 0, true);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.all_views AS v WHERE CAST(ISNULL(v.is_dropped_ledger_view, 0) AS bit) = 0 AND v.name = 'v'"), "1");
    exec(&mut c, "CREATE INDEX ix ON dbo.t (a)");
    assert_eq!(scalar(&mut c, "SELECT CAST(ISNULL(ic.column_store_order_ordinal, 0) AS int) FROM sys.index_columns AS ic JOIN sys.tables t ON t.object_id = ic.object_id WHERE t.name = 't'"), "0");
}

#[test]
fn what_the_table_designer_asks_at_open_works() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN permissions() <> 0 THEN 1 ELSE 0 END"), "1");
    assert_eq!(
        scalar(&mut c, "SELECT COUNT(*) FROM sys.database_files AS df INNER JOIN sys.databases AS dtb ON (db_id() = dtb.database_id) AND (df.type = 2 OR df.type = 0) AND (df.drop_lsn IS NULL) WHERE dtb.source_database_id IS NULL"),
        "1"
    );
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.master_files AS mf WHERE mf.type = 0 AND mf.drop_lsn IS NULL AND mf.state = 0 AND mf.database_id = db_id()"), "1");
}

#[test]
fn the_information_schema_views_clients_read_are_all_there() {
    // Beekeeper Studio (INFORMATION_SCHEMA.PARAMETERS) and others read the standard views; a missing one stopped the client
    let mut c = conn();
    exec(&mut c, "CREATE TABLE dbo.p (id int NOT NULL CONSTRAINT pk_p PRIMARY KEY, n varchar(10) CONSTRAINT ck_n CHECK (n <> 'x'))");
    exec(&mut c, "CREATE TABLE dbo.c (id int NOT NULL, pid int NOT NULL CONSTRAINT fk_c_p FOREIGN KEY REFERENCES dbo.p (id) ON DELETE CASCADE)");
    exec(&mut c, "CREATE VIEW dbo.v AS SELECT id, n FROM dbo.p");
    exec(&mut c, "CREATE PROCEDURE dbo.pr @a int, @b varchar(20) OUTPUT AS SELECT 1");
    exec(&mut c, "CREATE SEQUENCE dbo.sq AS int START WITH 5 INCREMENT BY 2");
    exec(&mut c, "CREATE TYPE dbo.d FROM varchar(30) NOT NULL");
    let n = |c: &mut tsql_server::Connection, v: &str| scalar(c, &format!("SELECT COUNT(*) FROM INFORMATION_SCHEMA.{}", v));
    assert_eq!(n(&mut c, "PARAMETERS"), "2");
    assert_eq!(scalar(&mut c, "SELECT PARAMETER_MODE + ':' + DATA_TYPE FROM INFORMATION_SCHEMA.PARAMETERS WHERE PARAMETER_NAME = '@b'"), "INOUT:varchar");
    assert_eq!(n(&mut c, "REFERENTIAL_CONSTRAINTS"), "1");
    assert_eq!(scalar(&mut c, "SELECT UNIQUE_CONSTRAINT_NAME + ':' + DELETE_RULE + ':' + UPDATE_RULE FROM INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS"), "pk_p:CASCADE:NO ACTION");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM INFORMATION_SCHEMA.CONSTRAINT_COLUMN_USAGE WHERE CONSTRAINT_NAME = 'fk_c_p' AND TABLE_NAME = 'p' AND COLUMN_NAME = 'id'"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM INFORMATION_SCHEMA.CONSTRAINT_TABLE_USAGE WHERE CONSTRAINT_NAME = 'pk_p'"), "1");
    assert_eq!(scalar(&mut c, "SELECT CHECK_CLAUSE FROM INFORMATION_SCHEMA.CHECK_CONSTRAINTS WHERE CONSTRAINT_NAME = 'ck_n'").is_empty(), false);
    assert_eq!(n(&mut c, "VIEW_TABLE_USAGE"), "1");
    assert_eq!(n(&mut c, "VIEW_COLUMN_USAGE"), "2");
    assert_eq!(scalar(&mut c, "SELECT DATA_TYPE FROM INFORMATION_SCHEMA.DOMAINS WHERE DOMAIN_NAME = 'd'"), "varchar");
    assert_eq!(scalar(&mut c, "SELECT START_VALUE FROM INFORMATION_SCHEMA.SEQUENCES WHERE SEQUENCE_NAME = 'sq'"), "5");
    // (nothing is granted here, and the table-valued functions' columns are not listed, but the views are there)
    for v in ["ROUTINE_COLUMNS", "TABLE_PRIVILEGES", "COLUMN_PRIVILEGES"] {
        assert_eq!(n(&mut c, v), "0", "{}", v);
    }
}

#[test]
fn the_partition_catalog_the_table_designer_reads_is_there() {
    let mut c = conn();
    assert_eq!(
        scalar(&mut c, "SELECT COUNT(*) FROM sys.partition_functions func LEFT OUTER JOIN (sys.partition_parameters para JOIN sys.types tp ON tp.user_type_id = para.system_type_id) ON para.function_id = func.function_id"),
        "0"
    );
    assert_eq!(
        scalar(&mut c, "SELECT COUNT(*) FROM sys.partition_schemes sch INNER JOIN (sys.destination_data_spaces dest INNER JOIN sys.filegroups fg ON fg.data_space_id = dest.data_space_id) ON dest.partition_scheme_id = sch.data_space_id"),
        "0"
    );
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.partition_range_values"), "0");
}

#[test]
fn the_column_query_of_the_table_designer_runs() {
    // COL_NAME of a NULL column id (a column that is not full-text) is NULL, not an error
    let mut c = conn();
    exec(&mut c, "CREATE TABLE dbo.t (a int IDENTITY(1,1) PRIMARY KEY, b nvarchar(10) DEFAULT 'x')");
    assert_eq!(scalar(&mut c, "SELECT ISNULL(col_name(OBJECT_ID('dbo.t'), NULL), 'none')"), "none");
    assert_eq!(
        scalar(
            &mut c,
            "SELECT COUNT(*) FROM (select col.name, col_name(col.object_id, ftc.type_column_id) FT_type_column, CONVERT(nvarchar(40), idc.seed_value) AS seed from sys.columns col \
             left outer join sys.identity_columns idc on idc.object_id = col.object_id and idc.column_id = col.column_id \
             left outer join sys.fulltext_index_columns ftc on ftc.object_id = col.object_id and ftc.column_id = col.column_id \
             where col.object_id = object_id(N'dbo.t')) x"
        ),
        "2"
    );
}

#[test]
fn constraints_are_objects_with_the_ids_their_own_views_give() {
    // the table designer asks `select id from sysobjects where id = object_id(N'DF_...')` for each default
    let mut c = conn();
    exec(&mut c, "CREATE TABLE dbo.t (a int NOT NULL CONSTRAINT pk_t PRIMARY KEY, b int CONSTRAINT df_t_b DEFAULT 5, c int CONSTRAINT uq_t_c UNIQUE, d int CONSTRAINT ck_t_d CHECK (d > 0))");
    exec(&mut c, "CREATE TABLE dbo.u (a int CONSTRAINT fk_u_t FOREIGN KEY REFERENCES dbo.t (a))");
    for (name, view, kind) in [("df_t_b", "sys.default_constraints", "D"), ("pk_t", "sys.key_constraints", "PK"), ("uq_t_c", "sys.key_constraints", "UQ"), ("ck_t_d", "sys.check_constraints", "C"), ("fk_u_t", "sys.foreign_keys", "F")] {
        let own = scalar(&mut c, &format!("SELECT object_id FROM {} WHERE name = '{}'", view, name));
        assert_eq!(scalar(&mut c, &format!("SELECT OBJECT_ID(N'{}')", name)), own, "{}", name);
        assert_eq!(scalar(&mut c, &format!("SELECT OBJECT_ID(N'dbo.{}')", name)), own, "dbo.{}", name);
        assert_eq!(scalar(&mut c, &format!("SELECT id FROM sysobjects WHERE id = OBJECT_ID(N'{}')", name)), own, "sysobjects {}", name);
        assert_eq!(scalar(&mut c, &format!("SELECT type FROM sys.objects WHERE object_id = {}", own)).trim(), kind, "sys.objects {}", name);
    }
    assert_eq!(scalar(&mut c, "SELECT ISNULL(OBJECT_ID(N'nope'), -1)"), "-1");
    // the properties of a constraint object (the table designer reads IsDefaultCnst of each column's default)
    assert_eq!(scalar(&mut c, "SELECT OBJECTPROPERTY(OBJECT_ID(N'df_t_b'), N'IsDefaultCnst')"), "1");
    assert_eq!(scalar(&mut c, "SELECT OBJECTPROPERTY(OBJECT_ID(N'pk_t'), N'IsDefaultCnst')"), "0");
    assert_eq!(scalar(&mut c, "SELECT OBJECTPROPERTY(OBJECT_ID(N'pk_t'), N'IsPrimaryKey')"), "1");
    assert_eq!(scalar(&mut c, "SELECT OBJECTPROPERTY(OBJECT_ID(N'fk_u_t'), N'IsForeignKey')"), "1");
}

#[test]
fn every_index_has_statistics_and_check_definitions_are_parenthesized() {
    // the index query of the table designer left-joins sys.stats: SQL Server has a row for each index (no_recompute 0, not NULL)
    let mut c = conn();
    exec(&mut c, "CREATE TABLE dbo.t (a int NOT NULL CONSTRAINT pk_t PRIMARY KEY, b int CONSTRAINT ck_b CHECK (b > 0))");
    exec(&mut c, "CREATE INDEX ix_b ON dbo.t (b)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.indexes ind LEFT OUTER JOIN sys.stats sta ON sta.object_id = ind.object_id AND sta.stats_id = ind.index_id WHERE ind.object_id = OBJECT_ID('dbo.t') AND sta.no_recompute = 0"), "2");
    assert_eq!(scalar(&mut c, "SELECT definition FROM sys.check_constraints WHERE name = 'ck_b'"), "(b > 0)");
}

#[test]
fn the_xml_index_query_of_the_table_designer_runs() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE dbo.t (a int NOT NULL PRIMARY KEY, x xml)");
    assert_eq!(
        scalar(
            &mut c,
            "select COUNT(*) from (sys.xml_indexes IndA left outer join sys.xml_indexes IndB on IndB.object_id = IndA.object_id and IndB.index_id = IndA.using_xml_index_id) \
             inner join (sys.internal_tables IntT inner join sys.stats st on IntT.internal_type = 202 and st.object_id = IntT.object_id) on IntT.parent_id = IndA.object_id and st.name = IndA.name \
             inner join sys.index_columns ind_col on ind_col.object_id = IndA.object_id and ind_col.index_id = IndA.index_id \
             where IndA.fill_factor = 0 and IndA.is_padded = 0 and IndA.is_disabled = 0 and IndA.is_hypothetical = 0 and IndA.allow_row_locks = 1 and IndA.allow_page_locks = 1 and IndA.object_id = object_id(N'dbo.t')"
        ),
        "0"
    );
}

#[test]
fn the_spatial_index_query_of_the_table_designer_runs() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE dbo.t (a int NOT NULL PRIMARY KEY, g geometry)");
    assert_eq!(
        scalar(
            &mut c,
            "select COUNT(*) from (sys.spatial_indexes IndA left outer join sys.spatial_index_tessellations IndB on IndB.object_id = IndA.object_id and IndB.index_id = IndA.index_id) \
             inner join (sys.internal_tables IntT inner join sys.stats st on st.object_id = IntT.object_id and st.stats_id = 1) on IntT.parent_id = IndA.object_id and IntT.parent_minor_id = IndA.index_id \
             where IndA.fill_factor = 0 and IndA.is_padded = 0 and IndA.is_disabled = 0 and IndA.is_hypothetical = 0 and IndA.allow_row_locks = 1 and IndA.allow_page_locks = 1 and IndA.object_id = object_id(N'dbo.t') \
             and isnull(IndB.level_1_grid, 16) = 16 and IndB.cells_per_object IS NULL"
        ),
        "0"
    );
}

#[test]
fn the_fulltext_index_query_of_the_table_designer_runs() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE dbo.t (a int NOT NULL CONSTRAINT pk_t PRIMARY KEY, b nvarchar(50))");
    assert_eq!(
        scalar(
            &mut c,
            "select count(*) from (select FtInd.unique_index_id, Ind.name as Index_Name, FtInd.fulltext_catalog_id, FtCat.name as Catalog_Name, FtInd.is_enabled, FtInd.change_tracking_state, FtInd.has_crawl_completed, FtInd.crawl_type, cast(0 as int) a, cast(0 as int) b, FtInd.crawl_start_date, FtInd.crawl_end_date, FtInd.incremental_timestamp \
             from sys.fulltext_indexes FtInd left outer join sys.fulltext_catalogs FtCat on FtCat.fulltext_catalog_id = FtInd.fulltext_catalog_id inner join sys.indexes Ind on Ind.object_id = FtInd.object_id and Ind.index_id = FtInd.unique_index_id where FtInd.object_id = object_id(N'dbo.t')) x"
        ),
        "0"
    );
}

#[test]
fn sysobjects_has_the_columns_the_designer_filters_on() {
    // the bound defaults and rules of the table designer: OBJECTPROPERTY(id, 'IsDefault') and the category bit of system objects
    let mut c = conn();
    exec(&mut c, "CREATE TABLE dbo.t (a int)");
    assert_eq!(scalar(&mut c, "select count(*) from sysobjects o where OBJECTPROPERTY(o.id, N'IsDefault') = 1 and o.name not like N'#%%' and o.category & 0x0800 = 0 and OBJECTPROPERTY(o.id, N'IsMSShipped') = 0"), "0");
    assert_eq!(scalar(&mut c, "select count(*) from sysobjects o where o.xtype = 'U' and o.status = 0 and o.base_schema_ver = 0"), "1");
}

#[test]
fn what_the_view_designer_asks_in_a_database_other_than_master_runs() {
    let mut c = conn();
    // the contained-authentication probe, run when the database is not master
    assert_eq!(scalar(&mut c, "select case when authenticating_database_id = 1 then 0 else 1 end from sys.dm_exec_sessions where session_id = @@SPID"), "0");
    // the full-text index of a table joined to its stoplist and search property list
    exec(&mut c, "CREATE TABLE dbo.t (a int NOT NULL CONSTRAINT pk_t PRIMARY KEY, b nvarchar(50))");
    assert_eq!(
        scalar(
            &mut c,
            "SELECT COUNT(*) FROM sys.tables AS tbl INNER JOIN sys.fulltext_indexes AS fti ON fti.object_id = tbl.object_id \
             LEFT OUTER JOIN sys.fulltext_stoplists AS sl ON sl.stoplist_id = fti.stoplist_id \
             LEFT OUTER JOIN sys.registered_search_property_lists AS spl ON spl.property_list_id = fti.property_list_id WHERE tbl.name = 't'"
        ),
        "0"
    );
}

#[test]
fn syscomments_has_the_text_of_views_and_procedures() {
    // the view designer reads the text of a view from it, by schema, name and colid
    let mut c = conn();
    exec(&mut c, "CREATE TABLE dbo.t (a int)");
    exec(&mut c, "CREATE VIEW dbo.v AS SELECT a FROM dbo.t");
    exec(&mut c, "CREATE PROCEDURE dbo.p AS SELECT 1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM (sys.schemas ss INNER JOIN sys.objects so ON ss.schema_id = so.schema_id) INNER JOIN syscomments sc ON so.object_id = sc.id WHERE (so.type = N'V') AND (ss.name = N'dbo') AND (so.name = N'v')"), "1");
    assert!(scalar(&mut c, "SELECT sc.text FROM sys.objects so INNER JOIN syscomments sc ON so.object_id = sc.id WHERE so.name = N'v' ORDER BY sc.colid").contains("SELECT a FROM dbo.t"));
    assert_eq!(scalar(&mut c, "SELECT DATALENGTH(sc.text) FROM sys.objects so INNER JOIN syscomments sc ON so.object_id = sc.id WHERE so.name = N'p'"), format!("{}", "CREATE PROCEDURE dbo.p AS SELECT 1".len() * 2));
    assert_eq!(scalar(&mut c, "SELECT sc.texttype FROM sys.objects so INNER JOIN syscomments sc ON so.object_id = sc.id WHERE so.name = N'v'"), "2");
}

#[test]
fn sp_fkeys_lists_the_foreign_keys_of_and_to_a_table() {
    // SSMS asks `EXEC sp_fkeys null, null, null, N'table', N'schema', N'db'` (the keys the table has) when it opens a table's design
    let mut c = conn();
    exec(&mut c, "CREATE TABLE dbo.p (id INT NOT NULL CONSTRAINT pk_p PRIMARY KEY, n INT)");
    exec(&mut c, "CREATE TABLE dbo.c (id INT PRIMARY KEY, pid INT CONSTRAINT fk_c_p REFERENCES dbo.p (id) ON DELETE CASCADE)");
    assert_eq!(rows(&mut c, "EXEC sp_fkeys null, null, null, N'c', N'dbo', N'master'").iter().map(|r| format!("{}.{}.{}->{}.{}:{}:{}:{}", r[1], r[2], r[3], r[5], r[6], r[7], r[11], r[10])).collect::<Vec<_>>(), vec!["dbo.p.id->dbo.c:pid:fk_c_p:0".to_string()]);
    assert_eq!(rows(&mut c, "EXEC sp_fkeys @pktable_name = N'p'").len(), 1);
    assert_eq!(rows(&mut c, "EXEC sp_fkeys N'c'").len(), 0);
    assert_eq!(rows(&mut c, "EXEC sp_fkeys N'p'")[0][12], "pk_p");
}

#[test]
fn what_ssms_asks_when_a_procedure_is_modified_runs() {
    // numbered procedures (proc;2) are not supported: the query for them finds none
    let mut c = conn();
    exec(&mut c, "CREATE PROCEDURE dbo.p @a int AS SELECT @a");
    assert_eq!(
        scalar(&mut c, "SELECT COUNT(*) FROM sys.all_objects AS sp INNER JOIN sys.numbered_procedures AS nsp ON nsp.object_id = sp.object_id WHERE (sp.type = N'P' OR sp.type = N'RF' OR sp.type = N'PC') AND sp.name = N'p' AND SCHEMA_NAME(sp.schema_id) = N'dbo'"),
        "0"
    );
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.numbered_procedure_parameters"), "0");
}

#[test]
fn the_properties_ssms_reads_of_a_procedure_to_script_it_are_there() {
    let mut c = conn();
    exec(&mut c, "CREATE PROCEDURE dbo.p @a int AS SELECT @a");
    let q = "SELECT sp.name, ISNULL(ssp.name, N'') AS [Owner], CAST(ISNULL(OBJECTPROPERTYEX(sp.object_id,N'ExecIsAnsiNullsOn'),0) AS bit), CAST(ISNULL(OBJECTPROPERTYEX(sp.object_id,N'ExecIsQuotedIdentOn'),0) AS bit), \
        CAST(ISNULL(OBJECTPROPERTYEX(sp.object_id, N'IsSchemaBound'),0) AS bit), CAST(CASE WHEN ISNULL(smsp.definition, ssmsp.definition) IS NULL THEN 1 ELSE 0 END AS bit), CAST(ISNULL(smsp.is_recompiled, ssmsp.is_recompiled) AS bit), \
        case when amsp.object_id is null then N'' else asmblsp.name end, case when amsp.object_id is null then N'' else amsp.assembly_class end, case when amsp.object_id is null then N'' else amsp.assembly_method end, \
        case when amsp.object_id is null then case isnull(smsp.execute_as_principal_id, -1) when -1 then 1 when -2 then 2 else 3 end else case isnull(amsp.execute_as_principal_id, -1) when -1 then 1 when -2 then 2 else 3 end end, \
        case when amsp.object_id is null then ISNULL(user_name(smsp.execute_as_principal_id),N'') else ISNULL(user_name(amsp.execute_as_principal_id), N'') end, CAST(ISNULL(spp.is_auto_executed,0) AS bit), \
        CASE WHEN sp.type = N'P' THEN 1 WHEN sp.type = N'PC' THEN 2 ELSE 1 END, CAST(CASE sp.type WHEN N'RF' THEN 1 ELSE 0 END AS bit), ISNULL(sm.uses_native_compilation,0) \
        FROM sys.all_objects AS sp LEFT OUTER JOIN sys.database_principals AS ssp ON ssp.principal_id = ISNULL(sp.principal_id, (OBJECTPROPERTY(sp.object_id, 'OwnerId'))) \
        LEFT OUTER JOIN sys.sql_modules AS smsp ON smsp.object_id = sp.object_id LEFT OUTER JOIN sys.system_sql_modules AS ssmsp ON ssmsp.object_id = sp.object_id \
        LEFT OUTER JOIN sys.assembly_modules AS amsp ON amsp.object_id = sp.object_id LEFT OUTER JOIN sys.assemblies AS asmblsp ON asmblsp.assembly_id = amsp.assembly_id \
        LEFT OUTER JOIN sys.procedures AS spp ON spp.object_id = sp.object_id LEFT OUTER JOIN sys.all_sql_modules AS sm ON sm.object_id = sp.object_id \
        WHERE (sp.type = N'P' OR sp.type = N'RF' OR sp.type = N'PC') AND sp.name = N'p' AND SCHEMA_NAME(sp.schema_id) = N'dbo'";
    assert_eq!(rows(&mut c, q).len(), 1);
}

#[test]
fn the_parameter_query_of_scripting_a_procedure_runs() {
    let mut c = conn();
    exec(&mut c, "CREATE PROCEDURE dbo.p @a int, @b varchar(10) = 'x' OUTPUT AS SELECT @a");
    assert_eq!(
        rows(&mut c, "SELECT param.parameter_id AS [ID], param.name AS [Name], param.default_value AS [DefaultValue], param.is_output AS [IsOutputParameter], param.is_readonly AS [IsReadOnly], usrt.name AS [DataType] \
                      FROM sys.all_objects AS sp INNER JOIN sys.all_parameters AS param ON param.object_id = sp.object_id LEFT OUTER JOIN sys.types AS usrt ON usrt.user_type_id = param.user_type_id \
                      WHERE sp.type = N'P' AND sp.name = N'p' ORDER BY [ID] ASC")
            .iter()
            .map(|r| format!("{}:{}:{}:{}", r[0], r[1], r[3], r[5]))
            .collect::<Vec<_>>(),
        vec!["1:@a:0:int".to_string(), "2:@b:1:varchar".to_string()]
    );
}

#[test]
fn the_fulltext_column_query_of_scripting_a_table_runs() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE dbo.t (a int NOT NULL CONSTRAINT pk_t PRIMARY KEY, b nvarchar(50))");
    assert_eq!(scalar(&mut c, "SELECT name FROM sys.fulltext_languages WHERE lcid = 1033"), "English");
    assert_eq!(
        scalar(
            &mut c,
            "SELECT COUNT(*) FROM sys.tables AS tbl INNER JOIN sys.fulltext_indexes AS fti ON fti.object_id = tbl.object_id INNER JOIN sys.fulltext_index_columns AS icol ON icol.object_id = fti.object_id \
             INNER JOIN sys.columns AS col ON col.object_id = icol.object_id AND col.column_id = icol.column_id LEFT OUTER JOIN sys.columns AS col2 ON col2.column_id = icol.type_column_id AND col2.object_id = icol.object_id \
             INNER JOIN sys.fulltext_languages AS sl ON sl.lcid = icol.language_id WHERE tbl.name = 't'"
        ),
        "0"
    );
}

#[test]
fn the_database_properties_ssms_reads_to_script_a_table_are_there() {
    let mut c = conn();
    assert_eq!(
        scalar(
            &mut c,
            "SELECT COUNT(*) FROM master.sys.databases AS dtb LEFT OUTER JOIN sys.database_recovery_status AS drs ON drs.database_id = dtb.database_id \
             LEFT OUTER JOIN sys.change_tracking_databases AS ctb ON ctb.database_id = dtb.database_id LEFT OUTER JOIN sys.database_filestream_options AS fsopt ON fsopt.database_id = dtb.database_id \
             WHERE dtb.name = N'master' AND drs.last_log_backup_lsn IS NULL AND ISNULL(fsopt.non_transacted_access, 0) = 0 AND ctb.retention_period IS NULL"
        ),
        "1"
    );
}

#[test]
fn smo_finds_every_configuration_row_it_reads_to_script_a_table() {
    // SMO reads the rows for these five options by position: a missing one is "There is no row at position 1"
    let mut c = conn();
    assert_eq!(
        rows(&mut c, "select cfg.configuration_id from sys.configurations as cfg where cfg.configuration_id in (115, 124, 1126, 1127, 1555) order by cfg.configuration_id asc").iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec!["115", "124", "1126", "1127", "1555"]
    );
    assert_eq!(scalar(&mut c, "select name from sys.fulltext_languages as fl where fl.lcid = (select value_in_use from sys.configurations where configuration_id = 1126)"), "English");
}

#[test]
fn a_database_role_has_an_owner_for_the_script_role_query() {
    // SSMS's Script Role joins the role to its owner (dbo owns the fixed roles and the roles made here): no row is "Object reference not set"
    let mut c = conn();
    exec(&mut c, "CREATE ROLE auditors");
    assert_eq!(
        rows(&mut c, "SELECT rl.name, ou.name FROM sys.database_principals AS rl INNER JOIN sys.database_principals AS ou ON ou.principal_id = rl.owning_principal_id WHERE (rl.type = 'R') AND rl.name IN (N'db_datareader', N'auditors') ORDER BY rl.name"),
        vec![vec!["auditors".to_string(), "dbo".into()], vec!["db_datareader".to_string(), "dbo".into()]]
    );
}

#[test]
fn the_users_node_of_ssms_sees_that_each_user_may_connect() {
    let mut c = conn();
    exec(&mut c, "CREATE USER bob WITHOUT LOGIN");
    exec(&mut c, "CREATE TABLE dbo.t (a int)");
    exec(&mut c, "GRANT SELECT ON dbo.t TO bob");
    assert_eq!(
        rows(&mut c, "SELECT u.name, CAST(CASE dp.state WHEN N'G' THEN 1 WHEN 'W' THEN 1 ELSE 0 END AS bit) FROM sys.database_principals AS u LEFT OUTER JOIN sys.database_permissions AS dp ON dp.grantee_principal_id = u.principal_id AND dp.type = N'CO' WHERE u.type in ('U', 'S', 'G', 'C', 'K', 'E', 'X') AND u.name = N'bob'"),
        vec![vec!["bob".to_string(), "1".into()]]
    );
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.database_permissions WHERE grantee_principal_id = USER_ID('bob') AND type = 'SL' AND state = 'G' AND class = 1"), "1");
}

#[test]
fn the_folder_picker_of_ssms_lists_drives_and_folders() {
    let mut c = conn();
    // the batch SSMS sends first: the view is announced in sys.all_objects and lists the drives
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.all_objects WHERE name='dm_os_enumerate_fixed_drives' AND type='V' AND is_ms_shipped=1"), "1");
    script(
        &mut c,
        "CREATE TABLE #fixdrv (Name sysname NOT NULL, Size bigint NOT NULL, Type sysname NULL);
         INSERT #fixdrv SELECT fixed_drive_path, free_space_in_bytes/(1024*1024), drive_type_desc FROM sys.dm_os_enumerate_fixed_drives;",
    );
    assert!(scalar(&mut c, "SELECT COUNT(*) FROM #fixdrv").parse::<i64>().unwrap() >= 1);
    // the older route, and the folder listings
    script(&mut c, "CREATE TABLE #d (Name sysname, Size bigint); INSERT #d (Name, Size) EXECUTE master.dbo.xp_fixeddrives;");
    let dir = std::env::temp_dir().join("tsql_picker_test");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let p = dir.display().to_string();
    script(&mut c, &format!("CREATE TABLE #s (n nvarchar(260)); INSERT #s EXEC master.dbo.xp_subdirs N'{p}';"));
    assert_eq!(scalar(&mut c, "SELECT n FROM #s"), "sub");
    script(&mut c, &format!("CREATE TABLE #e (a int, b int, c int); INSERT #e EXEC master.dbo.xp_fileexist N'{p}';"));
    assert_eq!(rows(&mut c, "SELECT a, b, c FROM #e"), vec![vec!["0".to_string(), "1".into(), "1".into()]]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_file_pickers_of_ssms_enumerate_the_servers_folders() {
    let mut c = conn();
    let dir = std::env::temp_dir().join("tsql_picker_test2");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("a.mdf"), b"x").unwrap();
    let p = dir.display().to_string();
    // the batch of the Locate Folder dialog
    script(&mut c, &format!(
        "CREATE TABLE #f (Name nvarchar(255) NOT NULL, IsFile bit NULL, FullName nvarchar(300) NOT NULL);
         DECLARE @Path nvarchar(255) = N'{p}';
         INSERT #f SELECT file_or_directory_name, 1 - is_directory, full_filesystem_path FROM sys.dm_os_enumerate_filesystem(@Path, '*') WHERE [level] = 0;"
    ));
    assert_eq!(rows(&mut c, "SELECT Name, IsFile FROM #f ORDER BY IsFile, Name"), vec![vec!["sub".to_string(), "0".into()], vec!["a.mdf".to_string(), "1".into()]]);
    assert_eq!(scalar(&mut c, &format!("SELECT COUNT(*) FROM sys.dm_os_enumerate_filesystem(N'{p}', '*.mdf')")), "1");
    assert_eq!(rows(&mut c, &format!("SELECT file_exists, file_is_a_directory, parent_directory_exists FROM sys.dm_os_file_exists(N'{p}')")), vec![vec!["0".to_string(), "1".into(), "1".into()]]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn msdb_backup_history_tables_exist_for_the_ssms_restore_dialog() {
    let mut c = conn();
    for t in ["backupset", "backupmediafamily", "backupmediaset", "backupfile", "backupfilegroup", "restorehistory", "restorefile", "restorefilegroup", "suspect_pages"] {
        assert_eq!(scalar(&mut c, &format!("SELECT COUNT(*) FROM msdb.dbo.{t}")), "0", "{t}");
    }
    // the forms SSMS uses
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM msdb..backupset WHERE type = 'D' OR type = 'L'"), "0");
    script(&mut c, "CREATE TABLE #tempbackup (database_name nvarchar(128), [type] char(1), backup_finish_date datetime); INSERT INTO #tempbackup SELECT database_name, [type], MAX(backup_finish_date) FROM msdb..backupset WHERE [type] = 'D' GROUP BY database_name, [type];");
    assert_eq!(rows(&mut c, "SELECT bkps.database_name FROM msdb.dbo.backupset bkps ORDER BY bkps.database_name").len(), 0);
}

#[test]
fn syscolumns_of_sql_server_2000_lists_the_columns_of_the_tables() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE sc (id INT PRIMARY KEY, name VARCHAR(20) NOT NULL, note NVARCHAR(10) NULL)");
    // (Reporting Services' upgrade scripts: is a column there?)
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM dbo.syscolumns WHERE id = object_id('sc') AND name = 'note'"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM dbo.syscolumns WHERE id = object_id('sc') AND name = 'nope'"), "0");
    assert_eq!(rows(&mut c, "SELECT name, colid, isnullable FROM syscolumns WHERE id = OBJECT_ID('sc') ORDER BY colid"), vec![vec!["id", "1", "0"], vec!["name", "2", "0"], vec!["note", "3", "1"]]);
}

#[test]
fn sysindexes_of_sql_server_2000_lists_the_indexes_of_the_tables() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE si (id INT PRIMARY KEY, a INT, b INT)");
    exec(&mut c, "CREATE INDEX ix_a ON si (a)");
    exec(&mut c, "CREATE TABLE heap (x INT)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM dbo.sysindexes WHERE id = object_id('si') AND name = 'ix_a'"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sysindexes WHERE id = object_id('si') AND name = 'ix_none'"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sysindexes WHERE id = object_id('si') AND indid = 1"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sysindexes WHERE id = object_id('heap') AND indid = 0"), "1");
}

#[test]
fn sysmembers_of_sql_server_2000_lists_the_members_of_the_roles() {
    let mut c = conn();
    exec(&mut c, "EXEC sp_addrole 'RSExecRole'");
    let q = "SELECT COUNT(*) FROM sysusers u JOIN sysmembers m ON u.uid = m.memberuid JOIN sysusers r ON r.uid = m.groupuid WHERE r.name = 'db_owner' AND u.name = 'RSExecRole'";
    assert_eq!(scalar(&mut c, q), "0");
    exec(&mut c, "EXEC sp_addrolemember 'db_owner', 'RSExecRole'");
    assert_eq!(scalar(&mut c, q), "1");
}

#[test]
fn syslogins_of_sql_server_2000_lists_the_logins() {
    let mut c = conn();
    // (Reporting Services' setup)
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM syslogins WHERE name = N'sa' AND hasaccess = 1 AND isntgroup = 0 AND isntuser = 0"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM master.dbo.syslogins WHERE name = N'nobody'"), "0");
    exec(&mut c, "CREATE LOGIN ann WITH PASSWORD = 'Str0ng!pw1'");
    assert_eq!(scalar(&mut c, "SELECT sysadmin FROM syslogins WHERE name = 'ann'"), "0");
    exec(&mut c, "ALTER LOGIN ann DISABLE");
    assert_eq!(scalar(&mut c, "SELECT hasaccess FROM syslogins WHERE name = 'ann'"), "0");
}

#[test]
fn msdb_has_the_sql_server_agent_tables_and_they_are_empty() {
    let mut c = conn();
    // (Reporting Services' upgrade script joins its schedules to the jobs)
    exec(&mut c, "CREATE TABLE schedule (scheduleId UNIQUEIDENTIFIER PRIMARY KEY)");
    exec(&mut c, "INSERT schedule VALUES (NEWID())");
    for t in ["sysjobs", "syscategories", "sysjobschedules", "sysschedules", "sysjobsteps", "sysjobservers", "sysjobhistory"] {
        assert_eq!(scalar(&mut c, &format!("SELECT COUNT(*) FROM msdb.dbo.{}", t)), "0", "{}", t);
    }
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM schedule s JOIN msdb.dbo.sysjobs j ON (CONVERT(sysname, s.scheduleid) = j.name)"), "0");
}

#[test]
fn only_a_database_with_a_memory_optimized_table_shows_its_filegroup_and_file() {
    let mut c = conn();
    let both = "SELECT (SELECT COUNT(*) FROM sys.filegroups WHERE type = 'FX') + (SELECT COUNT(*) FROM sys.database_files WHERE type = 2)";
    assert_eq!(scalar(&mut c, both), "0");
    exec(&mut c, "CREATE TABLE m (id INT NOT NULL PRIMARY KEY NONCLUSTERED) WITH (MEMORY_OPTIMIZED = ON, DURABILITY = SCHEMA_ONLY)");
    assert_eq!(scalar(&mut c, both), "2");
}
