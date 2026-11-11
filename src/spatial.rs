//! The `geometry` and `geography` types: well-known text, the binary form SQL Server stores, and the common methods.
//!
//! A value is `Value::Bin` holding SQL Server's serialization (see below). The two types share one serialization, so a value
//! carries its kind inside the bytes: the top 12 bits of the SRID field hold a tag (`GEOMETRY_TAG`, `GEOGRAPHY_TAG`; real SRIDs
//! are below 2^20, and a binary value that is not one of these has the tag and a version byte of 1 only by chance). The tag is added when a value is made or converted to one of the types and removed when it is
//! converted to binary or sent to a client, so bytes that leave the server are SQL Server's.
//!
//! **Serialization** (version 1 of SQL Server's format): SRID (4 bytes), version `01`, properties (`0C` a single point, `14` a
//! line of two points, otherwise `04`), then for a single point or segment just the points, else the number of points, the
//! points, the number of figures, the figures (attribute and first point), the number of shapes and the shapes (parent, first
//! figure, type). A point is two doubles, `(x, y)` for a geometry and `(latitude, longitude)` for a geography. The single-point
//! form is verified against the 19,614 points of the AdventureWorks sample; the general form is written from the published
//! layout and has not been checked against SQL Server output, because the sample holds no other shape.
//!
//! **Supported:** points, linestrings, polygons (with holes), their multi forms and collections, in two dimensions. Z and M
//! values, circular arcs and `FULLGLOBE` are errors. **Geometry** is planar: length, area, distance, intersection, containment.
//! **Geography** uses the WGS 84 ellipsoid for lengths and for the distance between points (Vincenty's formulae, as SQL Server
//! does, to the millimetre); the distance between other shapes and areas use a sphere and are approximations; `STIntersects`,
//! `STContains` and `STWithin` are not supported for geography (they are errors, not planar guesses).

const GEOMETRY_TAG: u32 = 0xA5C;
const GEOGRAPHY_TAG: u32 = 0xA5D;
const SRID_MASK: u32 = 0xF_FFFF;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Geometry,
    Geography,
}

/// A coordinate as written in text: `(x, y)`, for a geography `(longitude, latitude)`.
pub type P = (f64, f64);

#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    Point(Option<P>),
    Line(Vec<P>),
    Polygon(Vec<Vec<P>>),
    MultiPoint(Vec<P>),
    MultiLine(Vec<Vec<P>>),
    MultiPolygon(Vec<Vec<Vec<P>>>),
    Collection(Vec<Shape>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Spatial {
    pub kind: Kind,
    pub srid: i32,
    pub shape: Shape,
}

/// Is this the internal form of a geometry or geography?
pub fn kind_of(bytes: &[u8]) -> Option<Kind> {
    if bytes.len() < 6 {
        return None;
    }
    if bytes[4] != 1 {
        return None;
    }
    match u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) >> 20 {
        GEOGRAPHY_TAG => Some(Kind::Geography),
        GEOMETRY_TAG => Some(Kind::Geometry),
        _ => None,
    }
}

/// SQL Server's bytes for an internal value (the flag removed).
pub fn external(bytes: &[u8]) -> Vec<u8> {
    let mut b = bytes.to_vec();
    if kind_of(bytes).is_some() {
        let srid = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) & SRID_MASK;
        b[..4].copy_from_slice(&srid.to_le_bytes());
    }
    b
}

fn num(f: f64) -> String {
    if f == f.trunc() && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{}", f)
    }
}

// ----------------------------------------------------------------------------------------------------------- text

struct Lex<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Lex<'a> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(format!("expected '{}' at position {}", c as char, self.i + 1))
        }
    }
    fn word(&mut self) -> String {
        self.ws();
        let st = self.i;
        while self.i < self.s.len() && self.s[self.i].is_ascii_alphabetic() {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[st..self.i]).to_uppercase()
    }
    fn number(&mut self) -> Result<f64, String> {
        self.ws();
        let st = self.i;
        while self.i < self.s.len() && (self.s[self.i].is_ascii_digit() || matches!(self.s[self.i], b'+' | b'-' | b'.' | b'e' | b'E')) {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[st..self.i]).ok().and_then(|t| t.parse::<f64>().ok()).filter(|f| f.is_finite()).ok_or_else(|| format!("a number was expected at position {}", st + 1))
    }
    fn coord(&mut self) -> Result<P, String> {
        let x = self.number()?;
        let y = self.number()?;
        self.ws();
        if self.s.get(self.i).is_some_and(|c| c.is_ascii_digit() || *c == b'-' || *c == b'+') {
            return Err("coordinates with Z or M values are not supported".into());
        }
        Ok((x, y))
    }
    /// `(a, b, c)` with `item` for each element.
    fn list<T>(&mut self, item: &mut dyn FnMut(&mut Self) -> Result<T, String>) -> Result<Vec<T>, String> {
        self.expect(b'(')?;
        let mut out = vec![item(self)?];
        while self.eat(b',') {
            out.push(item(self)?);
        }
        self.expect(b')')?;
        Ok(out)
    }
    fn empty(&mut self) -> bool {
        let save = self.i;
        if self.word() == "EMPTY" {
            true
        } else {
            self.i = save;
            false
        }
    }
    fn coords(&mut self) -> Result<Vec<P>, String> {
        self.list(&mut |l| l.coord())
    }
    fn rings(&mut self) -> Result<Vec<Vec<P>>, String> {
        self.list(&mut |l| l.coords())
    }
    fn shape(&mut self) -> Result<Shape, String> {
        let w = self.word();
        self.ws();
        if matches!(self.s.get(self.i), Some(b'Z' | b'z' | b'M' | b'm')) {
            return Err("coordinates with Z or M values are not supported".into());
        }
        let e = self.empty();
        Ok(match w.as_str() {
            "POINT" => Shape::Point(if e { None } else { Some(self.list(&mut |l| l.coord())?.remove(0)) }),
            "LINESTRING" => Shape::Line(if e { vec![] } else { self.coords()? }),
            "POLYGON" => Shape::Polygon(if e { vec![] } else { self.rings()? }),
            "MULTIPOINT" => Shape::MultiPoint(if e {
                vec![]
            } else {
                self.list(&mut |l| {
                    if l.eat(b'(') {
                        let p = l.coord()?;
                        l.expect(b')')?;
                        Ok(p)
                    } else {
                        l.coord()
                    }
                })?
            }),
            "MULTILINESTRING" => Shape::MultiLine(if e { vec![] } else { self.rings()? }),
            "MULTIPOLYGON" => Shape::MultiPolygon(if e { vec![] } else { self.list(&mut |l| l.rings())? }),
            "GEOMETRYCOLLECTION" => Shape::Collection(if e { vec![] } else { self.list(&mut |l| l.shape())? }),
            "CIRCULARSTRING" | "COMPOUNDCURVE" | "CURVEPOLYGON" | "FULLGLOBE" => return Err(format!("{} is not supported", w)),
            _ => return Err(format!("the text '{}' is not a known type of spatial object", String::from_utf8_lossy(self.s).chars().take(30).collect::<String>())),
        })
    }
}

/// Well-known text to a shape.
pub fn parse_wkt(text: &str) -> Result<Shape, String> {
    let mut l = Lex { s: text.as_bytes(), i: 0 };
    let s = l.shape()?;
    l.ws();
    if l.i != l.s.len() {
        return Err(format!("unexpected text at position {}", l.i + 1));
    }
    Ok(s)
}

fn pt(p: &P) -> String {
    format!("{} {}", num(p.0), num(p.1))
}
fn pts(v: &[P]) -> String {
    format!("({})", v.iter().map(pt).collect::<Vec<_>>().join(", "))
}

impl Shape {
    pub fn wkt(&self) -> String {
        fn rings(v: &[Vec<P>]) -> String {
            format!("({})", v.iter().map(|r| pts(r)).collect::<Vec<_>>().join(", "))
        }
        match self {
            Shape::Point(None) => "POINT EMPTY".into(),
            Shape::Point(Some(p)) => format!("POINT ({})", pt(p)),
            Shape::Line(v) if v.is_empty() => "LINESTRING EMPTY".into(),
            Shape::Line(v) => format!("LINESTRING {}", pts(v)),
            Shape::Polygon(v) if v.is_empty() => "POLYGON EMPTY".into(),
            Shape::Polygon(v) => format!("POLYGON {}", rings(v)),
            Shape::MultiPoint(v) if v.is_empty() => "MULTIPOINT EMPTY".into(),
            Shape::MultiPoint(v) => format!("MULTIPOINT ({})", v.iter().map(|p| format!("({})", pt(p))).collect::<Vec<_>>().join(", ")),
            Shape::MultiLine(v) if v.is_empty() => "MULTILINESTRING EMPTY".into(),
            Shape::MultiLine(v) => format!("MULTILINESTRING {}", rings(v)),
            Shape::MultiPolygon(v) if v.is_empty() => "MULTIPOLYGON EMPTY".into(),
            Shape::MultiPolygon(v) => format!("MULTIPOLYGON ({})", v.iter().map(|p| rings(p)).collect::<Vec<_>>().join(", ")),
            Shape::Collection(v) if v.is_empty() => "GEOMETRYCOLLECTION EMPTY".into(),
            Shape::Collection(v) => format!("GEOMETRYCOLLECTION ({})", v.iter().map(|s| s.wkt()).collect::<Vec<_>>().join(", ")),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Shape::Point(_) => "Point",
            Shape::Line(_) => "LineString",
            Shape::Polygon(_) => "Polygon",
            Shape::MultiPoint(_) => "MultiPoint",
            Shape::MultiLine(_) => "MultiLineString",
            Shape::MultiPolygon(_) => "MultiPolygon",
            Shape::Collection(_) => "GeometryCollection",
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Shape::Point(p) => p.is_none(),
            Shape::Line(v) | Shape::MultiPoint(v) => v.is_empty(),
            Shape::Polygon(v) | Shape::MultiLine(v) => v.is_empty(),
            Shape::MultiPolygon(v) => v.is_empty(),
            Shape::Collection(v) => v.iter().all(|s| s.is_empty()),
        }
    }

    /// Every point, in order.
    pub fn points(&self) -> Vec<P> {
        let mut out = vec![];
        self.visit(&mut |s| match s {
            Shape::Point(Some(p)) => out.push(*p),
            Shape::Line(v) | Shape::MultiPoint(v) => out.extend(v),
            Shape::Polygon(r) | Shape::MultiLine(r) => r.iter().for_each(|x| out.extend(x)),
            Shape::MultiPolygon(ps) => ps.iter().for_each(|r| r.iter().for_each(|x| out.extend(x))),
            _ => {}
        });
        out
    }

    fn visit(&self, f: &mut dyn FnMut(&Shape)) {
        match self {
            Shape::Collection(v) => v.iter().for_each(|s| s.visit(f)),
            other => f(other),
        }
    }

    /// The line strings (and rings' edges are in `rings`), the polygons, and the lone points.
    fn parts(&self) -> Parts {
        let mut p = Parts::default();
        self.visit(&mut |s| match s {
            Shape::Point(Some(x)) => p.points.push(*x),
            Shape::MultiPoint(v) => p.points.extend(v),
            Shape::Line(v) => p.lines.push(v.clone()),
            Shape::MultiLine(v) => p.lines.extend(v.iter().cloned()),
            Shape::Polygon(r) => p.polys.push(r.clone()),
            Shape::MultiPolygon(ps) => p.polys.extend(ps.iter().cloned()),
            _ => {}
        });
        p
    }
}

#[derive(Default)]
struct Parts {
    points: Vec<P>,
    lines: Vec<Vec<P>>,
    polys: Vec<Vec<Vec<P>>>,
}

impl Parts {
    /// Every segment, the edges of the rings included.
    fn segments(&self) -> Vec<(P, P)> {
        let mut out = vec![];
        for l in self.lines.iter().chain(self.polys.iter().flatten()) {
            out.extend(l.windows(2).map(|w| (w[0], w[1])));
        }
        out
    }
    fn vertices(&self) -> Vec<P> {
        let mut v = self.points.clone();
        v.extend(self.lines.iter().flatten());
        v.extend(self.polys.iter().flatten().flatten());
        v
    }
}

// ----------------------------------------------------------------------------------------------------- serialization

fn put_pt(out: &mut Vec<u8>, p: &P, kind: Kind) {
    let (a, b) = if kind == Kind::Geography { (p.1, p.0) } else { (p.0, p.1) };
    out.extend(a.to_le_bytes());
    out.extend(b.to_le_bytes());
}

struct Ser {
    pts: Vec<P>,
    figs: Vec<(u8, u32)>,
    shapes: Vec<(i32, i32, u8)>,
}

impl Ser {
    fn fig(&mut self, attr: u8, v: &[P]) {
        self.figs.push((attr, self.pts.len() as u32));
        self.pts.extend(v);
    }
    fn emit(&mut self, s: &Shape, parent: i32) {
        let me = self.shapes.len() as i32;
        let fo = self.figs.len() as i32;
        let ty = match s {
            Shape::Point(_) => 1,
            Shape::Line(_) => 2,
            Shape::Polygon(_) => 3,
            Shape::MultiPoint(_) => 4,
            Shape::MultiLine(_) => 5,
            Shape::MultiPolygon(_) => 6,
            Shape::Collection(_) => 7,
        };
        if s.is_empty() && !matches!(s, Shape::Collection(v) if !v.is_empty()) {
            self.shapes.push((parent, -1, ty));
            return;
        }
        self.shapes.push((parent, fo, ty));
        let poly = |me: &mut Ser, rings: &[Vec<P>]| {
            for (i, r) in rings.iter().enumerate() {
                me.fig(if i == 0 { 2 } else { 0 }, r);
            }
        };
        match s {
            Shape::Point(Some(p)) => self.fig(1, &[*p]),
            Shape::Line(v) => self.fig(1, v),
            Shape::Polygon(r) => poly(self, r),
            Shape::MultiPoint(v) => {
                for p in v {
                    let fo = self.figs.len() as i32;
                    self.fig(1, &[*p]);
                    self.shapes.push((me, fo, 1));
                }
            }
            Shape::MultiLine(v) => {
                for l in v {
                    let fo = self.figs.len() as i32;
                    self.fig(1, l);
                    self.shapes.push((me, fo, 2));
                }
            }
            Shape::MultiPolygon(ps) => {
                for r in ps {
                    let fo = self.figs.len() as i32;
                    poly(self, r);
                    self.shapes.push((me, fo, 3));
                }
            }
            Shape::Collection(v) => {
                for c in v {
                    self.emit(c, me);
                }
            }
            _ => {}
        }
    }
}

impl Spatial {
    /// SQL Server's bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = (self.srid as u32 & SRID_MASK).to_le_bytes().to_vec();
        out.push(1);
        match &self.shape {
            Shape::Point(Some(p)) => {
                out.push(0x0C);
                put_pt(&mut out, p, self.kind);
            }
            Shape::Line(v) if v.len() == 2 => {
                out.push(0x14);
                v.iter().for_each(|p| put_pt(&mut out, p, self.kind));
            }
            s => {
                out.push(0x04);
                let mut ser = Ser { pts: vec![], figs: vec![], shapes: vec![] };
                ser.emit(s, -1);
                out.extend((ser.pts.len() as u32).to_le_bytes());
                ser.pts.iter().for_each(|p| put_pt(&mut out, p, self.kind));
                out.extend((ser.figs.len() as u32).to_le_bytes());
                for (a, o) in &ser.figs {
                    out.push(*a);
                    out.extend(o.to_le_bytes());
                }
                out.extend((ser.shapes.len() as u32).to_le_bytes());
                for (p, f, t) in &ser.shapes {
                    out.extend(p.to_le_bytes());
                    out.extend(f.to_le_bytes());
                    out.push(*t);
                }
            }
        }
        out
    }

    /// The internal form: the bytes with the kind flag in the SRID.
    pub fn to_internal(&self) -> Vec<u8> {
        let mut b = self.to_bytes();
        let tag = if self.kind == Kind::Geography { GEOGRAPHY_TAG } else { GEOMETRY_TAG };
        let srid = (self.srid as u32 & SRID_MASK) | tag << 20;
        b[..4].copy_from_slice(&srid.to_le_bytes());
        b
    }

    /// Read SQL Server's bytes (or the internal form); `hint` is the kind when the bytes do not say.
    pub fn from_bytes(b: &[u8], hint: Kind) -> Result<Spatial, String> {
        let bad = || "the bytes are not a geometry or geography this server can read".to_string();
        if b.len() < 6 {
            return Err(bad());
        }
        let raw = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let kind = kind_of(b).unwrap_or(hint);
        let srid = (raw & SRID_MASK) as i32;
        if b[4] != 1 {
            return Err("only version 1 of the serialization is supported (no circular arcs)".into());
        }
        let props = b[5];
        if props & 0x03 != 0 {
            return Err("Z and M values are not supported".into());
        }
        let mut at = 6usize;
        let rd = |at: &mut usize, n: usize| -> Result<&[u8], String> {
            let s = b.get(*at..*at + n).ok_or_else(bad)?;
            *at += n;
            Ok(s)
        };
        let f8 = |s: &[u8]| f64::from_le_bytes(s.try_into().unwrap());
        let u4 = |s: &[u8]| u32::from_le_bytes(s.try_into().unwrap());
        let i4 = |s: &[u8]| i32::from_le_bytes(s.try_into().unwrap());
        let point = |at: &mut usize| -> Result<P, String> {
            let a = f8(rd(at, 8)?);
            let c = f8(rd(at, 8)?);
            Ok(if kind == Kind::Geography { (c, a) } else { (a, c) })
        };
        let shape = if props & 0x08 != 0 {
            Shape::Point(Some(point(&mut at)?))
        } else if props & 0x10 != 0 {
            Shape::Line(vec![point(&mut at)?, point(&mut at)?])
        } else {
            let n = u4(rd(&mut at, 4)?) as usize;
            let mut points = vec![];
            for _ in 0..n {
                points.push(point(&mut at)?);
            }
            let nf = u4(rd(&mut at, 4)?) as usize;
            let mut figs = vec![];
            for _ in 0..nf {
                let a = rd(&mut at, 1)?[0];
                figs.push((a, u4(rd(&mut at, 4)?) as usize));
            }
            let ns = u4(rd(&mut at, 4)?) as usize;
            let mut shapes = vec![];
            for _ in 0..ns {
                let p = i4(rd(&mut at, 4)?);
                let f = i4(rd(&mut at, 4)?);
                let t = rd(&mut at, 1)?[0];
                shapes.push((p, f, t));
            }
            if shapes.is_empty() {
                return Err(bad());
            }
            // the points of figure `f`
            let fig_pts = |f: usize| -> Vec<P> {
                let end = figs.get(f + 1).map(|x| x.1).unwrap_or(points.len());
                points.get(figs[f].1..end.min(points.len())).map(|s| s.to_vec()).unwrap_or_default()
            };
            // the figures of a leaf shape
            let fig_range = |i: usize| -> Vec<usize> {
                let (_, fo, _) = shapes[i];
                if fo < 0 {
                    return vec![];
                }
                let end = shapes.get(i + 1).filter(|s| s.1 >= 0).map(|s| s.1 as usize).unwrap_or(figs.len());
                (fo as usize..end.min(figs.len())).collect()
            };
            fn build(i: usize, shapes: &[(i32, i32, u8)], fp: &dyn Fn(usize) -> Vec<P>, fr: &dyn Fn(usize) -> Vec<usize>) -> Result<Shape, String> {
                let kids: Vec<usize> = (0..shapes.len()).filter(|&k| shapes[k].0 == i as i32 && k != i).collect();
                let line = |fs: Vec<usize>| fs.into_iter().flat_map(|f| fp(f)).collect::<Vec<P>>();
                Ok(match shapes[i].2 {
                    1 => Shape::Point(line(fr(i)).first().copied()),
                    2 => Shape::Line(line(fr(i))),
                    3 => Shape::Polygon(fr(i).into_iter().map(fp).collect()),
                    4 => Shape::MultiPoint(kids.iter().filter_map(|&k| line(fr(k)).first().copied()).collect()),
                    5 => Shape::MultiLine(kids.iter().map(|&k| line(fr(k))).collect()),
                    6 => Shape::MultiPolygon(kids.iter().map(|&k| fr(k).into_iter().map(fp).collect()).collect()),
                    7 => Shape::Collection(kids.iter().map(|&k| build(k, shapes, fp, fr)).collect::<Result<_, _>>()?),
                    _ => return Err("this kind of shape is not supported".into()),
                })
            }
            build(0, &shapes, &fig_pts, &fig_range)?
        };
        Ok(Spatial { kind, srid, shape })
    }
}

// ---------------------------------------------------------------------------------------------------------- checks

/// The checks SQL Server makes when a geography is made: latitude and longitude in range.
pub fn check_geography(shape: &Shape) -> Result<(), String> {
    for (x, y) in shape.points() {
        if !(-90.0..=90.0).contains(&y) {
            return Err(format!("24202: Latitude values must be between -90 and 90 degrees. ({})", num(y)));
        }
        if !(-15069.0..=15069.0).contains(&x) {
            return Err(format!("24204: Longitude values must be between -15069 and 15069 degrees. ({})", num(x)));
        }
    }
    Ok(())
}

/// A polygon needs closed rings of at least four points.
pub fn is_valid(shape: &Shape) -> bool {
    let ring_ok = |r: &Vec<P>| r.len() >= 4 && r.first() == r.last();
    let mut ok = true;
    shape.visit(&mut |s| match s {
        Shape::Polygon(rs) => ok &= rs.iter().all(ring_ok),
        Shape::MultiPolygon(ps) => ok &= ps.iter().flatten().all(ring_ok),
        Shape::Line(v) => ok &= v.is_empty() || v.len() >= 2,
        _ => {}
    });
    ok
}

// ------------------------------------------------------------------------------------------------------ geometry

fn dist(a: P, b: P) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn cross(o: P, a: P, b: P) -> f64 {
    (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
}

fn on_segment(p: P, a: P, b: P) -> bool {
    cross(a, b, p).abs() < 1e-12 * (1.0 + dist(a, b)) && p.0 >= a.0.min(b.0) - 1e-12 && p.0 <= a.0.max(b.0) + 1e-12 && p.1 >= a.1.min(b.1) - 1e-12 && p.1 <= a.1.max(b.1) + 1e-12
}

fn segs_intersect(a: P, b: P, c: P, d: P) -> bool {
    let (d1, d2, d3, d4) = (cross(c, d, a), cross(c, d, b), cross(a, b, c), cross(a, b, d));
    if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0)) && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0)) {
        return true;
    }
    on_segment(a, c, d) || on_segment(b, c, d) || on_segment(c, a, b) || on_segment(d, a, b)
}

fn seg_point_dist(p: P, a: P, b: P) -> f64 {
    let l2 = (b.0 - a.0).powi(2) + (b.1 - a.1).powi(2);
    if l2 == 0.0 {
        return dist(p, a);
    }
    let t = (((p.0 - a.0) * (b.0 - a.0) + (p.1 - a.1) * (b.1 - a.1)) / l2).clamp(0.0, 1.0);
    dist(p, (a.0 + t * (b.0 - a.0), a.1 + t * (b.1 - a.1)))
}

/// 1 inside, 0 on the boundary, -1 outside a ring.
fn in_ring(p: P, ring: &[P]) -> i32 {
    let mut inside = false;
    for w in ring.windows(2) {
        let (a, b) = (w[0], w[1]);
        if on_segment(p, a, b) {
            return 0;
        }
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0 {
            inside = !inside;
        }
    }
    if inside {
        1
    } else {
        -1
    }
}

/// Is the point in the polygon (outer ring, minus holes), the boundary counting as in?
fn in_poly(p: P, rings: &[Vec<P>]) -> bool {
    let Some(outer) = rings.first() else { return false };
    match in_ring(p, outer) {
        -1 => false,
        0 => true,
        _ => rings[1..].iter().all(|h| in_ring(p, h) != 1),
    }
}

fn ring_area(r: &[P]) -> f64 {
    r.windows(2).map(|w| w[0].0 * w[1].1 - w[1].0 * w[0].1).sum::<f64>() / 2.0
}

fn poly_area(rings: &[Vec<P>]) -> f64 {
    let mut it = rings.iter();
    let outer = it.next().map(|r| ring_area(r).abs()).unwrap_or(0.0);
    outer - it.map(|r| ring_area(r).abs()).sum::<f64>()
}

fn line_len(v: &[P], d: &dyn Fn(P, P) -> f64) -> f64 {
    v.windows(2).map(|w| d(w[0], w[1])).sum()
}

fn geometry_intersects(a: &Shape, b: &Shape) -> bool {
    let (pa, pb) = (a.parts(), b.parts());
    let (sa, sb) = (pa.segments(), pb.segments());
    let on_any = |p: P, segs: &[(P, P)]| segs.iter().any(|(x, y)| on_segment(p, *x, *y));
    if pa.points.iter().any(|p| pb.points.contains(p) || on_any(*p, &sb)) || pb.points.iter().any(|p| on_any(*p, &sa)) {
        return true;
    }
    if sa.iter().any(|(a1, a2)| sb.iter().any(|(b1, b2)| segs_intersect(*a1, *a2, *b1, *b2))) {
        return true;
    }
    // one inside the polygons of the other
    let inside = |vs: Vec<P>, polys: &[Vec<Vec<P>>]| vs.iter().any(|v| polys.iter().any(|r| in_poly(*v, r)));
    inside(pa.vertices(), &pb.polys) || inside(pb.vertices(), &pa.polys)
}

fn geometry_contains(a: &Shape, b: &Shape) -> bool {
    let (pa, pb) = (a.parts(), b.parts());
    if b.is_empty() {
        return false;
    }
    let mut test: Vec<P> = pb.vertices();
    test.extend(pb.segments().iter().map(|(x, y)| ((x.0 + y.0) / 2.0, (x.1 + y.1) / 2.0)));
    // where a has area, every point of b must be in it and in none of the holes; else b must lie on the lines and points of a
    if !pa.polys.is_empty() {
        return test.iter().all(|p| pa.polys.iter().any(|r| in_poly(*p, r)));
    }
    let sa = pa.segments();
    test.iter().all(|p| pa.points.contains(p) || sa.iter().any(|(x, y)| on_segment(*p, *x, *y))) && pb.polys.is_empty()
}

fn geometry_distance(a: &Shape, b: &Shape) -> f64 {
    if geometry_intersects(a, b) {
        return 0.0;
    }
    let (pa, pb) = (a.parts(), b.parts());
    let (sa, sb) = (pa.segments(), pb.segments());
    let (va, vb) = (pa.vertices(), pb.vertices());
    let mut best = f64::INFINITY;
    for p in &va {
        for q in &vb {
            best = best.min(dist(*p, *q));
        }
        for (x, y) in &sb {
            best = best.min(seg_point_dist(*p, *x, *y));
        }
    }
    for q in &vb {
        for (x, y) in &sa {
            best = best.min(seg_point_dist(*q, *x, *y));
        }
    }
    best
}

// ----------------------------------------------------------------------------------------------------- geography

const WGS_A: f64 = 6378137.0;
const WGS_F: f64 = 1.0 / 298.257223563;
const WGS_B: f64 = WGS_A * (1.0 - WGS_F);
const AUTHALIC_R: f64 = 6371007.1809;

/// The distance in metres between two (longitude, latitude) points on the WGS 84 ellipsoid (Vincenty's inverse formula).
pub fn geodesic(p: P, q: P) -> f64 {
    let (l1, l2) = (p.0.to_radians(), q.0.to_radians());
    let l = l2 - l1;
    let u1 = ((1.0 - WGS_F) * p.1.to_radians().tan()).atan();
    let u2 = ((1.0 - WGS_F) * q.1.to_radians().tan()).atan();
    let (su1, cu1, su2, cu2) = (u1.sin(), u1.cos(), u2.sin(), u2.cos());
    let mut lambda = l;
    let (mut ss, mut cs, mut sigma, mut c2a, mut c2sm) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for _ in 0..200 {
        let (sl, cl) = (lambda.sin(), lambda.cos());
        ss = ((cu2 * sl).powi(2) + (cu1 * su2 - su1 * cu2 * cl).powi(2)).sqrt();
        if ss == 0.0 {
            return 0.0;
        }
        cs = su1 * su2 + cu1 * cu2 * cl;
        sigma = ss.atan2(cs);
        let sa = cu1 * cu2 * sl / ss;
        c2a = 1.0 - sa * sa;
        c2sm = if c2a != 0.0 { cs - 2.0 * su1 * su2 / c2a } else { 0.0 };
        let c = WGS_F / 16.0 * c2a * (4.0 + WGS_F * (4.0 - 3.0 * c2a));
        let prev = lambda;
        lambda = l + (1.0 - c) * WGS_F * sa * (sigma + c * ss * (c2sm + c * cs * (-1.0 + 2.0 * c2sm * c2sm)));
        if (lambda - prev).abs() < 1e-12 {
            break;
        }
    }
    let u2s = c2a * (WGS_A * WGS_A - WGS_B * WGS_B) / (WGS_B * WGS_B);
    let a = 1.0 + u2s / 16384.0 * (4096.0 + u2s * (-768.0 + u2s * (320.0 - 175.0 * u2s)));
    let b = u2s / 1024.0 * (256.0 + u2s * (-128.0 + u2s * (74.0 - 47.0 * u2s)));
    let ds = b * ss * (c2sm + b / 4.0 * (cs * (-1.0 + 2.0 * c2sm * c2sm) - b / 6.0 * c2sm * (-3.0 + 4.0 * ss * ss) * (-3.0 + 4.0 * c2sm * c2sm)));
    WGS_B * a * (sigma - ds)
}

/// The distance from a point to a segment on a sphere (cross-track distance, clamped to the ends).
fn sphere_seg_dist(p: P, a: P, b: P) -> f64 {
    let d_ap = geodesic(a, p);
    let d_bp = geodesic(b, p);
    let r = AUTHALIC_R;
    let bearing = |f: P, t: P| {
        let (f1, f2, dl) = (f.1.to_radians(), t.1.to_radians(), (t.0 - f.0).to_radians());
        (dl.sin() * f2.cos()).atan2(f1.cos() * f2.sin() - f1.sin() * f2.cos() * dl.cos())
    };
    let ang = |d: f64| d / r;
    let (t13, t12) = (bearing(a, p), bearing(a, b));
    let xt = (ang(d_ap).sin() * (t13 - t12).sin()).asin().abs() * r;
    let along = (ang(d_ap).cos() / (xt / r).cos()).clamp(-1.0, 1.0).acos() * r;
    let len = geodesic(a, b);
    if (t13 - t12).cos() < 0.0 || along > len {
        d_ap.min(d_bp)
    } else {
        xt.min(d_ap).min(d_bp)
    }
}

fn geography_distance(a: &Shape, b: &Shape) -> f64 {
    let (pa, pb) = (a.parts(), b.parts());
    let (sa, sb) = (pa.segments(), pb.segments());
    let (va, vb) = (pa.vertices(), pb.vertices());
    let mut best = f64::INFINITY;
    for p in &va {
        for q in &vb {
            best = best.min(geodesic(*p, *q));
        }
        for (x, y) in &sb {
            best = best.min(sphere_seg_dist(*p, *x, *y));
        }
    }
    for q in &vb {
        for (x, y) in &sa {
            best = best.min(sphere_seg_dist(*q, *x, *y));
        }
    }
    best
}

/// The area of a polygon on a sphere with the authalic radius of the ellipsoid, in square metres.
fn sphere_ring_area(r: &[P]) -> f64 {
    let s: f64 = r.windows(2).map(|w| (w[1].0 - w[0].0).to_radians() * (2.0 + w[0].1.to_radians().sin() + w[1].1.to_radians().sin())).sum();
    (s * AUTHALIC_R * AUTHALIC_R / 2.0).abs()
}

// ------------------------------------------------------------------------------------------------------- methods

impl Spatial {
    pub fn text(&self) -> String {
        self.shape.wkt()
    }

    pub fn num_points(&self) -> i32 {
        self.shape.points().len() as i32
    }

    pub fn length(&self) -> f64 {
        let d: &dyn Fn(P, P) -> f64 = if self.kind == Kind::Geography { &geodesic } else { &dist };
        let mut t = 0.0;
        let p = self.shape.parts();
        t += p.lines.iter().map(|l| line_len(l, d)).sum::<f64>();
        t += p.polys.iter().flatten().map(|l| line_len(l, d)).sum::<f64>();
        t
    }

    pub fn area(&self) -> f64 {
        let polys = self.shape.parts().polys;
        if self.kind == Kind::Geography {
            polys
                .iter()
                .map(|rs| {
                    let mut it = rs.iter();
                    it.next().map(|r| sphere_ring_area(r)).unwrap_or(0.0) - it.map(|r| sphere_ring_area(r)).sum::<f64>()
                })
                .sum()
        } else {
            polys.iter().map(|r| poly_area(r)).sum()
        }
    }

    pub fn distance(&self, other: &Spatial) -> f64 {
        if self.kind == Kind::Geography {
            geography_distance(&self.shape, &other.shape)
        } else {
            geometry_distance(&self.shape, &other.shape)
        }
    }

    pub fn intersects(&self, other: &Spatial) -> Result<bool, String> {
        if self.kind == Kind::Geography {
            return Err("STIntersects is not supported for geography".into());
        }
        Ok(geometry_intersects(&self.shape, &other.shape))
    }

    pub fn contains(&self, other: &Spatial) -> Result<bool, String> {
        if self.kind == Kind::Geography {
            return Err("STContains and STWithin are not supported for geography".into());
        }
        Ok(geometry_contains(&self.shape, &other.shape))
    }

    pub fn num_geometries(&self) -> i32 {
        match &self.shape {
            _ if self.shape.is_empty() => 0,
            Shape::MultiPoint(v) => v.len() as i32,
            Shape::MultiLine(v) => v.len() as i32,
            Shape::MultiPolygon(v) => v.len() as i32,
            Shape::Collection(v) => v.len() as i32,
            _ => 1,
        }
    }

    /// The n-th point (1-based) of a line, the first and last points being `STStartPoint` and `STEndPoint`.
    pub fn point_n(&self, n: i64) -> Option<Spatial> {
        let pts = match &self.shape {
            Shape::Line(v) => v.clone(),
            _ => return None,
        };
        let p = pts.get((n - 1).max(0) as usize).filter(|_| n >= 1)?;
        Some(Spatial { kind: self.kind, srid: self.srid, shape: Shape::Point(Some(*p)) })
    }

    pub fn end_point(&self) -> Option<Spatial> {
        match &self.shape {
            Shape::Line(v) if !v.is_empty() => self.point_n(v.len() as i64),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(k: Kind, wkt: &str) -> Spatial {
        Spatial { kind: k, srid: 4326, shape: parse_wkt(wkt).unwrap() }
    }

    #[test]
    fn text_round_trips() {
        for t in [
            "POINT (1 2)",
            "POINT EMPTY",
            "LINESTRING (0 0, 1 1, 2.5 -3)",
            "POLYGON ((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 2 1, 2 2, 1 1))",
            "MULTIPOINT ((1 2), (3 4))",
            "MULTILINESTRING ((0 0, 1 1), (2 2, 3 3, 4 4))",
            "MULTIPOLYGON (((0 0, 1 0, 1 1, 0 0)), ((5 5, 6 5, 6 6, 5 5)))",
            "GEOMETRYCOLLECTION (POINT (1 2), LINESTRING (0 0, 1 1, 2 2), POLYGON ((0 0, 1 0, 1 1, 0 0)))",
            "GEOMETRYCOLLECTION EMPTY",
        ] {
            assert_eq!(parse_wkt(t).unwrap().wkt(), t);
        }
        // spacing and case do not matter; the output is normal
        assert_eq!(parse_wkt("point(1   2)").unwrap().wkt(), "POINT (1 2)");
        assert_eq!(parse_wkt("MULTIPOINT(1 2, 3 4)").unwrap().wkt(), "MULTIPOINT ((1 2), (3 4))");
        assert!(parse_wkt("POINT (1 2 3)").is_err() && parse_wkt("POINT Z (1 2 3)").is_err() && parse_wkt("POINT (1)").is_err() && parse_wkt("BLOB (1 2)").is_err() && parse_wkt("POINT (1 2) x").is_err());
    }

    #[test]
    fn the_stored_form_of_a_point_is_sql_servers() {
        // SQL Server: geography::Point(47.65100, -122.34900, 4326)
        let p = Spatial { kind: Kind::Geography, srid: 4326, shape: Shape::Point(Some((-122.349, 47.651))) };
        let b = p.to_bytes();
        assert_eq!(&b[..6], &[0xE6, 0x10, 0x00, 0x00, 0x01, 0x0C]);
        assert_eq!(f64::from_le_bytes(b[6..14].try_into().unwrap()), 47.651, "latitude first");
        assert_eq!(f64::from_le_bytes(b[14..22].try_into().unwrap()), -122.349);
        assert_eq!(Spatial::from_bytes(&b, Kind::Geography).unwrap(), p);
        // the internal form carries the kind, and is read the same
        let i = p.to_internal();
        assert_eq!(kind_of(&i), Some(Kind::Geography));
        assert_eq!(external(&i), b);
        assert_eq!(Spatial::from_bytes(&i, Kind::Geometry).unwrap(), p);
        // a geometry point is (x, y)
        let q = Spatial { kind: Kind::Geometry, srid: 0, shape: Shape::Point(Some((1.0, 2.0))) };
        assert_eq!(f64::from_le_bytes(q.to_bytes()[6..14].try_into().unwrap()), 1.0);
    }

    #[test]
    fn every_shape_survives_the_binary_form() {
        for kind in [Kind::Geometry, Kind::Geography] {
            for t in [
                "POINT EMPTY",
                "LINESTRING (0 0, 1 1)",
                "LINESTRING (0 0, 1 1, 2 0)",
                "LINESTRING EMPTY",
                "POLYGON ((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 2 1, 2 2, 1 1))",
                "MULTIPOINT ((1 2), (3 4))",
                "MULTILINESTRING ((0 0, 1 1), (2 2, 3 3, 4 4))",
                "MULTIPOLYGON (((0 0, 1 0, 1 1, 0 0)), ((5 5, 6 5, 6 6, 5 5), (5.2 5.1, 5.5 5.1, 5.5 5.4, 5.2 5.1)))",
                "GEOMETRYCOLLECTION (POINT (1 2), LINESTRING (0 0, 1 1, 2 2), POLYGON ((0 0, 1 0, 1 1, 0 0)))",
                "GEOMETRYCOLLECTION (POINT EMPTY, MULTIPOINT ((1 2), (3 4)), LINESTRING (0 0, 1 1))",
            ] {
                let s = g(kind, t);
                let back = Spatial::from_bytes(&s.to_bytes(), kind).unwrap();
                assert_eq!(back.shape.wkt(), t, "{:?} {}", kind, t);
            }
        }
    }

    #[test]
    fn planar_measures_and_relations() {
        let sq = g(Kind::Geometry, "POLYGON ((0 0, 4 0, 4 4, 0 4, 0 0))");
        let holey = g(Kind::Geometry, "POLYGON ((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 2 1, 2 2, 1 2, 1 1))");
        assert_eq!(sq.area(), 16.0);
        assert_eq!(holey.area(), 15.0);
        assert_eq!(sq.length(), 16.0);
        assert_eq!(g(Kind::Geometry, "LINESTRING (0 0, 3 4)").length(), 5.0);
        assert_eq!(g(Kind::Geometry, "POINT (0 0)").distance(&g(Kind::Geometry, "POINT (3 4)")), 5.0);
        assert_eq!(g(Kind::Geometry, "POINT (7 2)").distance(&sq), 3.0);
        assert_eq!(g(Kind::Geometry, "POINT (2 2)").distance(&sq), 0.0);
        assert!(sq.intersects(&g(Kind::Geometry, "LINESTRING (-1 2, 5 2)")).unwrap());
        assert!(!sq.intersects(&g(Kind::Geometry, "POINT (5 5)")).unwrap());
        assert!(sq.contains(&g(Kind::Geometry, "POINT (1 1)")).unwrap());
        assert!(sq.contains(&g(Kind::Geometry, "LINESTRING (1 1, 3 3)")).unwrap());
        assert!(!sq.contains(&g(Kind::Geometry, "LINESTRING (1 1, 6 3)")).unwrap());
        assert!(!holey.contains(&g(Kind::Geometry, "POINT (1.5 1.5)")).unwrap());
        assert!(sq.contains(&sq).unwrap());
    }

    #[test]
    fn geography_distances_on_the_ellipsoid() {
        // Seattle to Portland: about 233 km
        let d = geodesic((-122.3321, 47.6062), (-122.6784, 45.5152));
        assert!((d - 234_000.0).abs() < 3_000.0, "{}", d);
        // one degree of latitude on the equator: 110574 m
        assert!((geodesic((0.0, 0.0), (0.0, 1.0)) - 110_574.4).abs() < 1.0);
        // one degree of longitude on the equator: 111319.49 m
        assert!((geodesic((0.0, 0.0), (1.0, 0.0)) - 111_319.49).abs() < 0.1);
        assert_eq!(geodesic((5.0, 5.0), (5.0, 5.0)), 0.0);
        // a point beside a line
        let line = g(Kind::Geography, "LINESTRING (0 0, 0 10)");
        let p = g(Kind::Geography, "POINT (1 5)");
        assert!((p.distance(&line) - 111_200.0).abs() < 1_500.0, "{}", p.distance(&line));
        assert!(g(Kind::Geography, "POLYGON ((0 0, 1 0, 1 1, 0 1, 0 0))").area() > 1.2e10);
        assert!(check_geography(&parse_wkt("POINT (0 91)").unwrap()).is_err());
        assert!(check_geography(&parse_wkt("POINT (20000 0)").unwrap()).is_err());
        assert!(g(Kind::Geography, "POLYGON ((0 0, 1 0, 1 1, 0 1, 0 0))").intersects(&g(Kind::Geography, "POINT (0 0)")).is_err());
    }
}
