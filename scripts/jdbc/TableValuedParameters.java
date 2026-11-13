import java.sql.*;
import com.microsoft.sqlserver.jdbc.*;
public class TableValuedParameters {
    public static void main(String[] a) throws Exception {
        try (Connection c = DriverManager.getConnection(a[0])) {
            try (Statement st = c.createStatement()) {
                st.execute("CREATE TYPE dbo.IdList AS TABLE (id INT NOT NULL PRIMARY KEY, name NVARCHAR(20) NULL)");
                st.execute("CREATE PROCEDURE dbo.p_sum @t dbo.IdList READONLY, @k INT AS SELECT SUM(id) + @k AS s, COUNT(*) AS n FROM @t");
            }
            SQLServerDataTable dt = new SQLServerDataTable();
            dt.addColumnMetadata("id", Types.INTEGER);
            dt.addColumnMetadata("name", Types.NVARCHAR);
            for (int i = 1; i <= 50; i++) dt.addRow(i, i % 5 == 0 ? null : "n" + i);
            try (SQLServerCallableStatement cs = (SQLServerCallableStatement) c.prepareCall("{call dbo.p_sum(?, ?)}")) {
                cs.setStructured(1, "dbo.IdList", dt);
                cs.setInt(2, 1000);
                try (ResultSet rs = cs.executeQuery()) { rs.next(); System.out.println("call: sum " + rs.getInt(1) + " count " + rs.getInt(2)); }
            }
            try (SQLServerPreparedStatement ps = (SQLServerPreparedStatement) c.prepareStatement("SELECT COUNT(*), MAX(name) FROM ? ")) {
                ps.setStructured(1, "dbo.IdList", dt);
            } catch (Exception e) { System.out.println("(a table as a FROM target is not valid T-SQL in JDBC either: " + e.getClass().getSimpleName() + ")"); }
            try (SQLServerPreparedStatement ps = (SQLServerPreparedStatement) c.prepareStatement("EXEC dbo.p_sum ?, ?")) {
                ps.setStructured(1, "dbo.IdList", dt);
                ps.setInt(2, 1);
                try (ResultSet rs = ps.executeQuery()) { rs.next(); System.out.println("exec: sum " + rs.getInt(1) + " count " + rs.getInt(2)); }
            }
        }
        System.out.println("done");
    }
}
