import java.sql.*;
public class Cursors {
    static void p(String s) { System.out.println(s); }
    static String q(Connection c, String sql) throws Exception { try (Statement st = c.createStatement(); ResultSet r = st.executeQuery(sql)) { r.next(); return r.getString(1); } }
    public static void main(String[] a) throws Exception {
        try (Connection c = DriverManager.getConnection(a[0]); Connection c2 = DriverManager.getConnection(a[0])) {
            try (Statement st = c.createStatement()) {
                st.execute("CREATE TABLE kt2 (id INT IDENTITY(1,1) PRIMARY KEY, name NVARCHAR(30), n INT)");
                for (int i = 1; i <= 20; i++) st.execute("INSERT kt2 (name, n) VALUES (N'row" + i + "', " + i + ")");
            }
            try (Statement st = c.createStatement(ResultSet.TYPE_SCROLL_SENSITIVE, ResultSet.CONCUR_UPDATABLE)) {
                st.setFetchSize(5);
                try (ResultSet rs = st.executeQuery("SELECT id, name, n FROM kt2 ORDER BY id")) {
                    p("type " + rs.getType() + " concurrency " + rs.getConcurrency());
                    rs.absolute(3);
                    rs.updateString("name", "changed");
                    rs.updateInt("n", 333);
                    rs.updateRow();
                    p("after updateRow: " + rs.getString("name") + " " + rs.getInt("n") + " | db: " + q(c2, "SELECT name + ':' + CAST(n AS VARCHAR(10)) FROM kt2 WHERE id = 3"));
                    try (Statement s2 = c2.createStatement()) { s2.execute("UPDATE kt2 SET n = 999 WHERE id = 5"); }
                    rs.absolute(5);
                    rs.refreshRow();
                    p("sees the other session's change: n = " + rs.getInt("n"));
                    rs.absolute(4);
                    rs.deleteRow();
                    p("row 4 deleted: " + rs.rowDeleted() + " | db count " + q(c2, "SELECT COUNT(*) FROM kt2"));
                    rs.moveToInsertRow();
                    rs.updateString("name", "inserted");
                    rs.updateInt("n", 777);
                    rs.insertRow();
                    p("inserted: db has " + q(c2, "SELECT COUNT(*) FROM kt2 WHERE name = N'inserted'"));
                    rs.moveToCurrentRow();
                    rs.last();
                    p("last row: " + rs.getString("name"));
                }
            }
            // a row that another session deleted is flagged deleted when the cursor reads it again
            try (Statement st = c.createStatement(ResultSet.TYPE_SCROLL_SENSITIVE, ResultSet.CONCUR_UPDATABLE)) {
                st.setFetchSize(3);
                try (ResultSet rs = st.executeQuery("SELECT id, name FROM kt2 ORDER BY id")) {
                    rs.absolute(2);
                    try (Statement s2 = c2.createStatement()) { s2.execute("DELETE FROM kt2 WHERE id = 8"); }
                    rs.absolute(7); // (row 4 is already gone, so id 8 is the 7th)
                    p("a row deleted elsewhere: rowDeleted " + rs.rowDeleted());
                    rs.absolute(8);
                    p("the next row: " + rs.rowDeleted() + " " + rs.getString("name"));
                }
            }
            try (Statement st = c.createStatement(ResultSet.TYPE_FORWARD_ONLY, ResultSet.CONCUR_UPDATABLE)) {
                try (ResultSet rs = st.executeQuery("SELECT id, name FROM kt2 WHERE id <= 2 ORDER BY id")) {
                    p("forward-only updatable: type " + rs.getType() + " concurrency " + rs.getConcurrency());
                    while (rs.next()) { rs.updateString("name", "fwd" + rs.getInt("id")); rs.updateRow(); }
                }
                p("after forward updates: " + q(c2, "SELECT STRING_AGG(name, ',') FROM kt2 WHERE id <= 2"));
            }
            try (Statement st = c.createStatement(ResultSet.TYPE_SCROLL_SENSITIVE, ResultSet.CONCUR_UPDATABLE)) {
                try (ResultSet rs = st.executeQuery("SELECT n % 3 AS g, COUNT(*) AS cnt FROM kt2 GROUP BY n % 3")) {
                    p("grouped: type " + rs.getType() + " concurrency " + rs.getConcurrency());
                    try { rs.next(); rs.updateInt("cnt", 1); rs.updateRow(); p("grouped result was updated?!"); } catch (SQLException e) { p("grouped result is not updatable: " + e.getMessage()); }
                }
            }
        }
        p("done");
    }
}
