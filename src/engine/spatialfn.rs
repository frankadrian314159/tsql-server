//! The methods of `geometry` and `geography` (see spatial.rs): `g.STAsText()`, `g.STDistance(h)`, `geography::Point(...)`.
//!
//! The parser turns `expr.Method(args)` into the call `HID_METHOD(expr, args...)` (the same route as the methods of
//! `hierarchyid`) and `geometry::Method(args)` into `SP_S_GEOMETRY_METHOD(args...)`.
use super::*;
use crate::spatial::{self as sp, Kind, Shape, Spatial};

/// The method names (without `HID_`) that belong to the spatial types; `ToString` is shared with hierarchyid.
pub const METHODS: &[&str] = &[
    "STASTEXT", "STSRID", "STX", "STY", "LAT", "LONG", "STGEOMETRYTYPE", "STNUMPOINTS", "STISEMPTY", "STISVALID", "STLENGTH", "STAREA", "STDISTANCE", "STINTERSECTS", "STCONTAINS", "STWITHIN", "STEQUALS", "STNUMGEOMETRIES",
    "STSTARTPOINT", "STENDPOINT", "STPOINTN",
];

/// Is this call one of the spatial methods? (`ToString` is theirs when the value is a geometry or geography.)
pub fn is_spatial_call(name: &str, args: &[Value]) -> bool {
    if name.starts_with("SP_S_") {
        return true;
    }
    match name.strip_prefix("HID_") {
        Some("TOSTRING") => matches!(args.first(), Some(Value::Bin(b)) if sp::kind_of(b).is_some()),
        Some(m) => METHODS.contains(&m),
        None => false,
    }
}

impl<'a> Engine<'a> {
    pub(crate) fn spatial_call(&mut self, name: &str, a: &[Value]) -> Result<Value> {
        if let Some(rest) = name.strip_prefix("SP_S_") {
            let (ty, method) = rest.split_once('_').unwrap_or((rest, ""));
            let kind = if ty == "GEOGRAPHY" { Kind::Geography } else { Kind::Geometry };
            return Self::spatial_static(kind, method, a);
        }
        let method = name.strip_prefix("HID_").unwrap_or(name);
        let Some(first) = a.first() else { return Err(SqlError::new(174, "The spatial method requires an instance.")) };
        if first.is_null() {
            return Ok(Value::Null);
        }
        let me = Self::spatial_arg(first, Kind::Geometry)?;
        let name_of = if me.kind == Kind::Geography { "geography" } else { "geometry" };
        let bad = |m: String| SqlError::new(6522, format!("A .NET Framework error occurred during execution of user-defined routine or aggregate \"{}\": {}.", name_of, m));
        let want = |n: usize| -> Result<()> {
            if a.len() != n + 1 {
                return Err(SqlError::new(174, format!("The {} method requires {} argument(s).", method.to_lowercase(), n)));
            }
            Ok(())
        };
        let flt = |f: f64| Value::Float(f);
        let other = |i: usize| -> Result<Option<Spatial>> {
            match &a[i] {
                Value::Null => Ok(None),
                v => Self::spatial_arg(v, me.kind).map(Some),
            }
        };
        Ok(match method {
            "TOSTRING" | "STASTEXT" => {
                want(0)?;
                Value::Str(me.text())
            }
            "STSRID" => {
                want(0)?;
                Value::Int(me.srid)
            }
            "STX" | "STY" | "LAT" | "LONG" => {
                want(0)?;
                match me.shape {
                    Shape::Point(Some((x, y))) => match (method, me.kind) {
                        ("STX", _) | ("LONG", Kind::Geography) => flt(x),
                        _ => flt(y),
                    },
                    _ => Value::Null,
                }
            }
            "STGEOMETRYTYPE" => {
                want(0)?;
                Value::Str(me.shape.type_name().into())
            }
            "STNUMPOINTS" => {
                want(0)?;
                Value::Int(me.num_points())
            }
            "STNUMGEOMETRIES" => {
                want(0)?;
                Value::Int(me.num_geometries())
            }
            "STISEMPTY" => {
                want(0)?;
                Value::Bit(me.shape.is_empty())
            }
            "STISVALID" => {
                want(0)?;
                Value::Bit(sp::is_valid(&me.shape))
            }
            "STLENGTH" => {
                want(0)?;
                flt(me.length())
            }
            "STAREA" => {
                want(0)?;
                flt(me.area())
            }
            "STSTARTPOINT" => {
                want(0)?;
                me.point_n(1).map(|p| Value::Bin(p.to_internal())).unwrap_or(Value::Null)
            }
            "STENDPOINT" => {
                want(0)?;
                me.end_point().map(|p| Value::Bin(p.to_internal())).unwrap_or(Value::Null)
            }
            "STPOINTN" => {
                want(1)?;
                match a[1].as_i64() {
                    Some(n) => me.point_n(n).map(|p| Value::Bin(p.to_internal())).unwrap_or(Value::Null),
                    None => Value::Null,
                }
            }
            "STDISTANCE" | "STINTERSECTS" | "STCONTAINS" | "STWITHIN" | "STEQUALS" => {
                want(1)?;
                let Some(o) = other(1)? else { return Ok(Value::Null) };
                // values with different spatial reference systems are not comparable
                if o.srid != me.srid || o.kind != me.kind {
                    return Ok(Value::Null);
                }
                match method {
                    "STDISTANCE" => {
                        if me.shape.is_empty() || o.shape.is_empty() {
                            Value::Null
                        } else {
                            flt(me.distance(&o))
                        }
                    }
                    "STINTERSECTS" => Value::Bit(me.intersects(&o).map_err(bad)?),
                    "STCONTAINS" => Value::Bit(me.contains(&o).map_err(bad)?),
                    "STWITHIN" => Value::Bit(o.contains(&me).map_err(bad)?),
                    _ => Value::Bit(me.shape == o.shape),
                }
            }
            other => return Err(SqlError::new(195, format!("'{}' is not a recognized built-in function name.", other))),
        })
    }

    /// A spatial value from an argument: the stored bytes, or text.
    fn spatial_arg(v: &Value, hint: Kind) -> Result<Spatial> {
        let bad = |m: String| SqlError::new(6522, format!("A .NET Framework error occurred during execution of user-defined routine or aggregate \"{}\": {}.", if hint == Kind::Geography { "geography" } else { "geometry" }, m));
        match v {
            Value::Bin(b) => Spatial::from_bytes(b, hint).map_err(bad),
            Value::Str(s) => {
                let shape = sp::parse_wkt(s).map_err(bad)?;
                Ok(Spatial { kind: hint, srid: if hint == Kind::Geography { 4326 } else { 0 }, shape })
            }
            other => Err(SqlError::new(8116, format!("Argument data type {} is invalid for argument of the spatial method.", other.type_label()))),
        }
    }

    /// `geometry::Point(x, y, srid)`, `geography::Point(lat, long, srid)`, `STGeomFromText(text, srid)`, `Parse(text)` ...
    fn spatial_static(kind: Kind, method: &str, a: &[Value]) -> Result<Value> {
        let tname = if kind == Kind::Geography { "geography" } else { "geometry" };
        let bad = |m: String| SqlError::new(6522, format!("A .NET Framework error occurred during execution of user-defined routine or aggregate \"{}\": {}.", tname, m));
        let make = |srid: i64, shape: Shape| -> Result<Value> {
            let ok = if kind == Kind::Geography { (1..=999_999).contains(&srid) } else { (0..=999_999).contains(&srid) };
            if !ok {
                return Err(bad("24204: The spatial reference identifier (SRID) is not valid. The specified SRID must be between 0 and 999999 (1 and 999999 for geography)".into()));
            }
            if kind == Kind::Geography {
                sp::check_geography(&shape).map_err(bad)?;
            }
            Ok(Value::Bin(Spatial { kind, srid: srid as i32, shape }.to_internal()))
        };
        let need = |n: usize| -> Result<()> {
            if a.len() != n {
                return Err(SqlError::new(174, format!("The {} method requires {} argument(s).", method.to_lowercase(), n)));
            }
            Ok(())
        };
        if a.iter().any(|v| v.is_null()) {
            return Ok(Value::Null);
        }
        let f = |v: &Value| convert::cast(v, &DataType::Float, None).map(|x| if let Value::Float(f) = x { f } else { 0.0 });
        match method {
            "POINT" => {
                need(3)?;
                let (p, q) = (f(&a[0])?, f(&a[1])?);
                let at = if kind == Kind::Geography { (q, p) } else { (p, q) };
                make(a[2].as_i64().unwrap_or(-1), Shape::Point(Some(at)))
            }
            "PARSE" => {
                need(1)?;
                let shape = sp::parse_wkt(&convert::to_str(&a[0])).map_err(bad)?;
                make(if kind == Kind::Geography { 4326 } else { 0 }, shape)
            }
            "STGEOMFROMTEXT" | "STPOINTFROMTEXT" | "STLINEFROMTEXT" | "STPOLYFROMTEXT" | "STMPOINTFROMTEXT" | "STMLINEFROMTEXT" | "STMPOLYFROMTEXT" | "STGEOMCOLLFROMTEXT" => {
                need(2)?;
                let shape = sp::parse_wkt(&convert::to_str(&a[0])).map_err(bad)?;
                let want = match method {
                    "STPOINTFROMTEXT" => Some("Point"),
                    "STLINEFROMTEXT" => Some("LineString"),
                    "STPOLYFROMTEXT" => Some("Polygon"),
                    "STMPOINTFROMTEXT" => Some("MultiPoint"),
                    "STMLINEFROMTEXT" => Some("MultiLineString"),
                    "STMPOLYFROMTEXT" => Some("MultiPolygon"),
                    "STGEOMCOLLFROMTEXT" => Some("GeometryCollection"),
                    _ => None,
                };
                if want.is_some_and(|w| w != shape.type_name()) {
                    return Err(bad(format!("24142: Expected a {} instance, found a {}", want.unwrap(), shape.type_name())));
                }
                make(a[1].as_i64().unwrap_or(-1), shape)
            }
            other => Err(SqlError::new(195, format!("'{}' is not a recognized {} method.", other, tname))),
        }
    }
}
