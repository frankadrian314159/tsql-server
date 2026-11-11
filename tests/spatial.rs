//! The geometry and geography types (see src/spatial.rs).
mod common;
use common::*;

fn first(c: &mut tsql_server::Connection, sql: &str) -> String {
    scalar(c, sql)
}

#[test]
fn geometry_construction_and_text() {
    let mut c = conn();
    assert_eq!(first(&mut c, "SELECT geometry::Point(1, 2, 0).STAsText()"), "POINT (1 2)");
    assert_eq!(first(&mut c, "SELECT geometry::STGeomFromText('linestring(0 0, 3 4)', 0).ToString()"), "LINESTRING (0 0, 3 4)");
    assert_eq!(first(&mut c, "SELECT geometry::Parse('POLYGON((0 0, 4 0, 4 4, 0 4, 0 0))').STGeometryType()"), "Polygon");
    assert_eq!(first(&mut c, "SELECT geometry::Point(1, 2, 25).STSrid"), "25");
    assert_eq!(first(&mut c, "SELECT geometry::Point(1.5, -2.25, 0).STX"), "1.5");
    assert_eq!(first(&mut c, "SELECT geometry::Point(1.5, -2.25, 0).STY"), "-2.25");
    assert_eq!(first(&mut c, "SELECT geometry::STGeomFromText('MULTIPOINT((1 2),(3 4))', 0).STNumPoints()"), "2");
    assert_eq!(first(&mut c, "SELECT geometry::STGeomFromText('POINT EMPTY', 0).STIsEmpty()"), "1");
    assert_eq!(first(&mut c, "SELECT geometry::STGeomFromText('LINESTRING(0 0, 1 1, 2 0)', 0).STNumPoints()"), "3");
    assert_eq!(first(&mut c, "SELECT geometry::STPointFromText('POINT(5 6)', 0).STX"), "5");
    // a point from a line text is refused, and so is bad text
    assert_eq!(err_no(&mut c, "SELECT geometry::STPointFromText('LINESTRING(0 0, 1 1)', 0)"), 6522);
    assert_eq!(err_no(&mut c, "SELECT geometry::Parse('POINT(1)')"), 6522);
    assert_eq!(err_no(&mut c, "SELECT geometry::Point(1, 2, -5)"), 6522);
    // NULL in, NULL out
    assert_eq!(first(&mut c, "SELECT geometry::Point(NULL, 2, 0).STAsText()"), "NULL");
}

#[test]
fn geometry_measures_and_relations() {
    let mut c = conn();
    exec(&mut c, "DECLARE @sq geometry = 'POLYGON((0 0, 4 0, 4 4, 0 4, 0 0))'; SELECT 1");
    let r = rows(
        &mut c,
        "DECLARE @sq geometry = geometry::STGeomFromText('POLYGON((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 2 1, 2 2, 1 2, 1 1))', 0);
         DECLARE @line geometry = geometry::STGeomFromText('LINESTRING(-1 2, 5 2)', 0);
         DECLARE @in geometry = geometry::Point(3, 3, 0), @out geometry = geometry::Point(7, 2, 0), @hole geometry = geometry::Point(1.5, 1.5, 0);
         SELECT @sq.STArea(), @sq.STLength(), @line.STLength(), @sq.STIntersects(@line), @sq.STContains(@in), @sq.STContains(@hole), @in.STWithin(@sq), @out.STDistance(@sq), @in.STDistance(@out), @sq.STIntersects(@out), @in.STEquals(geometry::Point(3, 3, 0));",
    );
    assert_eq!(r[0].join("|"), "15|20|6|1|1|0|1|3|4.123105625617661|0|1");
    // different SRIDs do not compare
    assert_eq!(first(&mut c, "SELECT geometry::Point(1, 1, 1).STDistance(geometry::Point(1, 1, 2))"), "NULL");
}

#[test]
fn geography_points_and_distances() {
    let mut c = conn();
    assert_eq!(first(&mut c, "SELECT geography::Point(47.65100, -122.34900, 4326).STAsText()"), "POINT (-122.349 47.651)");
    assert_eq!(first(&mut c, "SELECT geography::Point(47.65100, -122.34900, 4326).Lat"), "47.651");
    assert_eq!(first(&mut c, "SELECT geography::Point(47.65100, -122.34900, 4326).Long"), "-122.349");
    assert_eq!(first(&mut c, "SELECT geography::STGeomFromText('POINT(-122.349 47.651)', 4326).STSrid"), "4326");
    // metres on the WGS 84 ellipsoid: one degree of longitude on the equator
    assert_eq!(first(&mut c, "SELECT CAST(geography::Point(0, 0, 4326).STDistance(geography::Point(0, 1, 4326)) AS DECIMAL(12,2))"), "111319.49");
    let km = first(&mut c, "SELECT CAST(geography::Point(47.6062, -122.3321, 4326).STDistance(geography::Point(45.5152, -122.6784, 4326)) / 1000 AS INT)");
    assert!((km.parse::<i32>().unwrap() - 234).abs() <= 2, "{}", km);
    assert_eq!(first(&mut c, "SELECT CAST(geography::STGeomFromText('LINESTRING(0 0, 1 0)', 4326).STLength() AS DECIMAL(12,2))"), "111319.49");
    // range checks
    assert_eq!(err_no(&mut c, "SELECT geography::Point(91, 0, 4326)"), 6522);
    assert_eq!(err_no(&mut c, "SELECT geography::Point(0, 0, 0)"), 6522, "geography needs an SRID");
    assert_eq!(err_no(&mut c, "SELECT geography::Parse('POINT(0 0)').STIntersects(geography::Parse('POINT(0 0)'))"), 6522, "not supported");
    // a spatial value in a string, and as the bytes SQL Server stores
    assert_eq!(first(&mut c, "SELECT CAST(geography::Point(47.65100, -122.34900, 4326) AS VARCHAR(100))"), "POINT (-122.349 47.651)");
    assert_eq!(first(&mut c, "SELECT CONVERT(VARCHAR(100), CAST(geography::Point(1, 2, 4326) AS VARBINARY(100)), 2)"), "E6100000010C000000000000F03F0000000000000040");
}

#[test]
fn columns_variables_and_the_bytes_of_a_backup() {
    let mut c = conn();
    script(
        &mut c,
        "CREATE TABLE places (id INT PRIMARY KEY, name VARCHAR(20), loc geography NULL, area geometry NULL);
         INSERT INTO places VALUES (1, 'a', geography::Point(10, 20, 4326), geometry::Parse('POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))'));
         INSERT INTO places VALUES (2, 'b', geography::Point(11, 21, 4326), NULL);
         INSERT INTO places VALUES (3, 'c', NULL, NULL);
         -- the bytes SQL Server stores for geography::Point(1, 2, 4326), as a backup holds them
         INSERT INTO places VALUES (4, 'd', 0xE6100000010C000000000000F03F0000000000000040, NULL);",
    );
    let r = rows(&mut c, "SELECT id, loc.Lat, loc.Long, loc.STAsText() FROM places WHERE loc IS NOT NULL ORDER BY id");
    assert_eq!(r.iter().map(|x| x.join("|")).collect::<Vec<_>>(), vec!["1|10|20|POINT (20 10)", "2|11|21|POINT (21 11)", "4|1|2|POINT (2 1)"]);
    assert_eq!(first(&mut c, "SELECT p.area.STArea() FROM places p WHERE p.id = 1"), "4");
    assert_eq!(first(&mut c, "SELECT places.loc.Lat FROM places WHERE id = 2"), "11");
    // the nearest place to a point
    assert_eq!(first(&mut c, "SELECT TOP 1 name FROM places WHERE loc IS NOT NULL ORDER BY loc.STDistance(geography::Point(10.9, 20.9, 4326))"), "b");
    // the kind of a column is kept: the same bytes read as a geography by a geography column
    assert_eq!(first(&mut c, "SELECT loc.STGeometryType() FROM places WHERE id = 4"), "Point");
    assert_eq!(first(&mut c, "SELECT TYPE_NAME(user_type_id) FROM sys.columns WHERE object_id = OBJECT_ID('places') AND name = 'loc'"), "geography");
    assert_eq!(first(&mut c, "SELECT TYPE_NAME(user_type_id) FROM sys.columns WHERE object_id = OBJECT_ID('places') AND name = 'area'"), "geometry");
    // a column named Lat or Long still works as a column
    script(&mut c, "CREATE TABLE t2 (Lat FLOAT, Long FLOAT); INSERT INTO t2 VALUES (1.5, 2.5);");
    assert_eq!(first(&mut c, "SELECT t2.Lat FROM t2"), "1.5");
    assert_eq!(first(&mut c, "SELECT Long FROM t2"), "2.5");
}
