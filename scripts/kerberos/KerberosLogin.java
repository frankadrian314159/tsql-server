import java.sql.*;

/** Kerberos login through the Microsoft JDBC driver (JavaKerberos, the ticket cache made by kinit). */
public class KerberosLogin {
    static void p(String s) { System.out.println(s); }

    public static void main(String[] a) throws Exception {
        String host = "localhost:" + a[0];
        String spn = "serverSpn=MSSQLSvc/localhost:" + a[0] + "@TSQL.TEST";
        try (Connection sa = DriverManager.getConnection("jdbc:sqlserver://" + host + ";user=sa;password=Str0ng!pw;encrypt=false")) {
            try (Statement st = sa.createStatement()) {
                st.execute("CREATE LOGIN [TSQL\\alice] FROM WINDOWS");
                st.execute("ALTER SERVER ROLE sysadmin ADD MEMBER [TSQL\\alice]"); st.execute("CREATE TABLE kt (n INT)");
                st.execute("INSERT kt VALUES (1), (2), (3)");
                ResultSet rs = st.executeQuery("SELECT name, type FROM sys.server_principals WHERE name LIKE 'TSQL%'");
                while (rs.next()) p("principal " + rs.getString(1) + " type " + rs.getString(2));
            }
        }
        String url = "jdbc:sqlserver://" + host + ";integratedSecurity=true;authenticationScheme=JavaKerberos;" + spn + ";encrypt=false";
        try (Connection c = DriverManager.getConnection(url)) {
            try (Statement st = c.createStatement(); ResultSet rs = st.executeQuery("SELECT SUSER_SNAME(), (SELECT COUNT(*) FROM kt)")) {
                rs.next(); p("kerberos login as " + rs.getString(1) + ", rows " + rs.getInt(2));
            }
        }
        // the same ticket for a second login
        try (Connection c = DriverManager.getConnection(url)) { p("second login ok"); }
        p("done");
    }
}
