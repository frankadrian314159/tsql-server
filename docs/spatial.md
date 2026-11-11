# geometry and geography

`geometry` (planar) and `geography` (round-earth) are real types (`src/spatial.rs`, methods in `src/engine/spatialfn.rs`). A
value is `Value::Bin`; its text form is well-known text.

## Supported

* **Shapes**, in two dimensions: `POINT`, `LINESTRING`, `POLYGON` (with holes), `MULTIPOINT`, `MULTILINESTRING`,
  `MULTIPOLYGON`, `GEOMETRYCOLLECTION`, and `EMPTY` of each. Z and M values, `CIRCULARSTRING`, `COMPOUNDCURVE`, `CURVEPOLYGON` and
  `FULLGLOBE` are errors (6522).
* **Construction**: `geometry::Point(x, y, srid)`, `geography::Point(lat, long, srid)`, `Parse(text)`,
  `STGeomFromText(text, srid)`, and `STPointFromText`, `STLineFromText`, `STPolyFromText`, `STMPointFromText`,
  `STMLineFromText`, `STMPolyFromText`, `STGeomCollFromText` (which check the type). A geography is checked for latitude
  (-90 to 90) and longitude (-15069 to 15069) and needs an SRID of 1 to 999999; a geometry's is 0 to 999999. `CAST('POINT(1 2)' AS
  geometry)` has SRID 0, as a geography it has 4326.
* **Members**: `STAsText()`, `ToString()`, `STSrid`, `STX`,
  `STY`, `Lat`, `Long` (written without parentheses, on a variable, a column `loc.Lat` or `t.loc.Lat`, or a method result),
  `STGeometryType()`, `STNumPoints()`, `STNumGeometries()`, `STIsEmpty()`, `STIsValid()` (closed rings of four points; no check
  that rings do not cross), `STLength()`, `STArea()`, `STStartPoint()`, `STEndPoint()`, `STPointN(n)`, `STDistance(g)`,
  `STEquals(g)`, and for geometry `STIntersects(g)`, `STContains(g)`, `STWithin(g)`. Two values with different SRIDs give NULL
  from the methods that take two.
* **Conversions**: `CAST(g AS varchar)` is the text, `CAST(g AS varbinary)` is SQL Server's bytes, and bytes (as a backup holds
  them) cast to the type; a column of the type that is filled from bytes is checked.

## The numbers

* **Geometry** is exact planar geometry. `STContains` for a polygon tests the vertices and the middles of the edges of the
  other shape, which is exact for convex polygons and can be wrong for a concave one whose edge leaves it between two inside points.
* **Geography** lengths and the distance between two points use Vincenty's inverse formula on the WGS 84 ellipsoid, as SQL Server does
  (a millimetre or so; every SRID is treated as WGS 84). The distance between other shapes (a point and a line, two polygons) is
  the smallest distance between vertices and segments, with the segment case on a sphere of the ellipsoid's authalic radius,
  so it is approximate (well under 1%), and it is not 0 for a point inside a polygon. `STArea()` is on that sphere as well, and
  ignores the orientation of the rings. **`STIntersects`, `STContains` and `STWithin` are errors for geography** (planar
  results would be wrong).

## The binary form

SQL Server stores both types with one serialization: SRID (4 bytes), version `01`, properties, then points, figures and shapes
(see the comment at the top of `src/spatial.rs`). A single point (`0C`) and a line of two points (`14`) are written as SQL
Server writes them, and the point form is checked against the 19,614 geography points of the AdventureWorks sample. The general
form (more points, polygons, multi shapes, collections) is written from the published layout; it has not been compared with SQL
Server's own bytes because the sample has no other shape, and bytes that SQL Server wrote for those shapes may not be read.
Version 2 (circular arcs) is not read.

Because the two types share the bytes, a value carries its kind in the top 12 bits of the SRID field (a tag; real SRIDs are below
2^20). The tag is added when a value is made, cast to the type or stored in a column of the type, and removed when it is cast to
binary or sent to a client (TDS), so bytes leaving the server are SQL Server's. The shell shows an internal value with the tag still
set. A binary value that is not one of these is taken for one only if its first four bytes carry the tag and its fifth is 1, which
random bytes do about once in two million times; it then converts to text as a spatial value instead of as bytes.

## Not supported

`STBuffer`, `STUnion`, `STIntersection`, `STDifference`, `STConvexHull`, `STEnvelope`, `STCentroid`, `STGeometryN`,
`STInteriorRingN`, `STExteriorRing`, `STTouches`, `STOverlaps`, `STCrosses`, `STDisjoint`, `STRelate`, `STSrid` as a setter,
`STAsBinary`, `STGeomFromWKB`, GML and spatial indexes (`CREATE SPATIAL INDEX` is not parsed; AdventureWorks' spatial index is skipped on a
restore). `sys.spatial_reference_systems` does not exist.
