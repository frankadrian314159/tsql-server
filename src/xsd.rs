//! XML Schema (XSD) for typed `xml`: a schema collection is compiled from schema documents and an XML document is validated
//! against it (`Collection::validate`).
//!
//! **Supported.** Elements (global, local, `ref`, `nillable`, `default`, `fixed`, `minOccurs`/`maxOccurs`), attributes (`use`,
//! `default`, `fixed`, `ref`), complex types with `sequence`, `choice`, `all`, `group` references, `any` and `anyAttribute`
//! (`processContents` strict, lax, skip), mixed content, `simpleContent` and `complexContent` by extension and restriction,
//! `attributeGroup`; simple types by restriction (all the facets: `enumeration`, `pattern`, `length`, `minLength`,
//! `maxLength`, `minInclusive`, `maxInclusive`, `minExclusive`, `maxExclusive`, `totalDigits`, `fractionDigits`,
//! `whiteSpace`), `list` and `union`; the built-in types of XML Schema Part 2 (checked lexically; the range of the integer
//! types is enforced); target namespaces, `elementFormDefault` and `attributeFormDefault`; `xsi:nil` and `xsi:type`; several
//! schema documents in one collection (they may refer to each other's namespaces).
//!
//! **Not supported.** `redefine`, substitution groups (`substitutionGroup` and `block`/`final` are read and ignored),
//! identity constraints (`key`, `keyref`, `unique`: read and ignored), checking that a restriction really restricts its
//! base or that the content model is deterministic (unique particle attribution), `ID`/`IDREF` uniqueness and the
//! lexical forms of `QName` and `NOTATION` beyond their syntax. Patterns use the syntax of the `regex` crate (XSD's `\i`,
//! `\c` and the `\p{IsBlock}` classes are not available and make the schema fail to compile). `import` and `include` are
//! accepted and do not fetch anything: what a schema needs must be in the same collection.
//!
//! Validation reports the first error, with the numbers and the wording of SQL Server's messages where they are remembered
//! (6913 no declaration for an element, 6914 for an attribute, 6926 a value that is not valid for its simple type, 6965 content
//! that does not fit the content model, 6909 a missing attribute, 6910 a nil element that is not empty); none of them has been
//! compared with a real server.
use crate::xmlpath::{Doc, View};
use regex::Regex;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

pub const XS: &str = "http://www.w3.org/2001/XMLSchema";
pub const XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";

/// An expanded name: namespace (empty for none) and local part.
pub type Name = (String, String);

#[derive(Debug, Clone)]
pub struct XsdError {
    pub number: i32,
    pub message: String,
}

fn verr(number: i32, message: String) -> XsdError {
    XsdError { number, message }
}

macro_rules! re {
    ($p:expr) => {{
        static R: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        R.get_or_init(|| Regex::new($p).unwrap())
    }};
}

// ------------------------------------------------------------------------------------------------------------ model

#[derive(Debug, Clone)]
enum TypeSpec {
    Named(Name),
    Simple(Rc<SimpleType>),
    Complex(Rc<ComplexType>),
}

#[derive(Debug)]
enum SimpleKind {
    Restriction { base: TypeSpec, facets: Facets },
    List(TypeSpec),
    Union(Vec<TypeSpec>),
}

#[derive(Debug)]
struct SimpleType {
    kind: SimpleKind,
}

#[derive(Debug, Default)]
struct Facets {
    enumeration: Vec<String>,
    /// the patterns of this derivation step (a value must match one of them)
    patterns: Vec<Regex>,
    length: Option<usize>,
    min_length: Option<usize>,
    max_length: Option<usize>,
    min_inclusive: Option<String>,
    max_inclusive: Option<String>,
    min_exclusive: Option<String>,
    max_exclusive: Option<String>,
    total_digits: Option<usize>,
    fraction_digits: Option<usize>,
    whitespace: Option<Ws>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ws {
    Preserve,
    Replace,
    Collapse,
}

#[derive(Debug)]
struct ComplexType {
    mixed: bool,
    content: Content,
    attrs: Vec<AttrUse>,
    any_attr: Option<Wildcard>,
}

#[derive(Debug, Clone)]
enum Content {
    Empty,
    Simple(TypeSpec),
    Particle(Particle),
}

#[derive(Debug, Clone)]
struct Particle {
    min: u32,
    max: Option<u32>,
    term: Term,
}

#[derive(Debug, Clone)]
enum Term {
    Element(Rc<Element>),
    /// a reference to a global element
    Ref(Name),
    Seq(Vec<Particle>),
    Choice(Vec<Particle>),
    All(Vec<Particle>),
    Any(Wildcard),
}

#[derive(Debug)]
struct Element {
    name: Name,
    ty: Option<TypeSpec>,
    nillable: bool,
    default: Option<String>,
    fixed: Option<String>,
}

#[derive(Debug, Clone)]
struct Wildcard {
    /// `##any`, `##other`, or a list of namespaces (`##local` is the empty string, `##targetNamespace` the target)
    any: bool,
    other_than: Option<String>,
    namespaces: Vec<String>,
    process: Process,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Process {
    Strict,
    Lax,
    Skip,
}

#[derive(Debug, Clone)]
struct AttrUse {
    name: Name,
    ty: TypeSpec,
    required: bool,
    prohibited: bool,
    default: Option<String>,
    fixed: Option<String>,
}

enum Def {
    Simple(Rc<SimpleType>),
    Complex(Rc<ComplexType>),
}

/// A compiled schema collection.
pub struct Collection {
    elements: HashMap<Name, Rc<Element>>,
    types: HashMap<Name, Def>,
    attributes: HashMap<Name, AttrUse>,
    namespaces: Vec<String>,
}

// ------------------------------------------------------------------------------------------------------ built-ins

#[derive(Debug, Clone, Copy, PartialEq)]
enum Prim {
    Str,
    NormalizedString,
    Token,
    Language,
    Name,
    NcName,
    NmToken,
    AnyUri,
    QName,
    Boolean,
    Decimal,
    Integer,
    Float,
    Duration,
    DateTime,
    Time,
    Date,
    GYearMonth,
    GYear,
    GMonthDay,
    GDay,
    GMonth,
    HexBinary,
    Base64,
    AnySimple,
}

/// A built-in simple type: its primitive family and, for the integer types, its range.
fn builtin(local: &str) -> Option<(Prim, Option<(i128, i128)>)> {
    let int = |lo: i128, hi: i128| Some((Prim::Integer, Some((lo, hi))));
    Some(match local {
        "string" => (Prim::Str, None),
        "normalizedString" => (Prim::NormalizedString, None),
        "token" => (Prim::Token, None),
        "language" => (Prim::Language, None),
        "Name" => (Prim::Name, None),
        "NCName" | "ID" | "IDREF" | "ENTITY" => (Prim::NcName, None),
        "NMTOKEN" => (Prim::NmToken, None),
        "anyURI" => (Prim::AnyUri, None),
        "QName" | "NOTATION" => (Prim::QName, None),
        "boolean" => (Prim::Boolean, None),
        "decimal" => (Prim::Decimal, None),
        "float" | "double" => (Prim::Float, None),
        "duration" => (Prim::Duration, None),
        "dateTime" => (Prim::DateTime, None),
        "time" => (Prim::Time, None),
        "date" => (Prim::Date, None),
        "gYearMonth" => (Prim::GYearMonth, None),
        "gYear" => (Prim::GYear, None),
        "gMonthDay" => (Prim::GMonthDay, None),
        "gDay" => (Prim::GDay, None),
        "gMonth" => (Prim::GMonth, None),
        "hexBinary" => (Prim::HexBinary, None),
        "base64Binary" => (Prim::Base64, None),
        "anySimpleType" => (Prim::AnySimple, None),
        "integer" => (Prim::Integer, None),
        "nonPositiveInteger" => return int(i128::MIN, 0),
        "negativeInteger" => return int(i128::MIN, -1),
        "long" => return int(i64::MIN as i128, i64::MAX as i128),
        "int" => return int(i32::MIN as i128, i32::MAX as i128),
        "short" => return int(i16::MIN as i128, i16::MAX as i128),
        "byte" => return int(-128, 127),
        "nonNegativeInteger" => return int(0, i128::MAX),
        "unsignedLong" => return int(0, u64::MAX as i128),
        "unsignedInt" => return int(0, u32::MAX as i128),
        "unsignedShort" => return int(0, u16::MAX as i128),
        "unsignedByte" => return int(0, 255),
        "positiveInteger" => return int(1, i128::MAX),
        _ => return None,
    })
}

fn valid_date(y: &str, m: u32, d: u32) -> bool {
    let year: i64 = y.trim_start_matches('-').parse().unwrap_or(0);
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return false,
    };
    (1..=days).contains(&d)
}

/// Check a (whitespace-normalised) value against a primitive type's lexical space.
fn lexical(p: Prim, range: Option<(i128, i128)>, v: &str) -> Result<(), String> {
    let tz = r"(Z|[+-]\d{2}:\d{2})?";
    let ok = |b: bool| if b { Ok(()) } else { Err(format!("'{}' is not a valid value", v)) };
    match p {
        Prim::Str | Prim::NormalizedString | Prim::Token | Prim::AnyUri | Prim::AnySimple => Ok(()),
        Prim::Language => ok(re!(r"^[a-zA-Z]{1,8}(-[a-zA-Z0-9]{1,8})*$").is_match(v)),
        Prim::Name => ok(re!(r"^[\p{L}_:][\p{L}\p{N}_:.\-]*$").is_match(v)),
        Prim::NcName => ok(re!(r"^[\p{L}_][\p{L}\p{N}_.\-]*$").is_match(v)),
        Prim::NmToken => ok(re!(r"^[\p{L}\p{N}_:.\-]+$").is_match(v)),
        Prim::QName => ok(re!(r"^([\p{L}_][\p{L}\p{N}_.\-]*:)?[\p{L}_][\p{L}\p{N}_.\-]*$").is_match(v)),
        Prim::Boolean => ok(matches!(v, "true" | "false" | "1" | "0")),
        Prim::Decimal => ok(re!(r"^[+-]?(\d+(\.\d*)?|\.\d+)$").is_match(v)),
        Prim::Integer => {
            if !re!(r"^[+-]?\d+$").is_match(v) {
                return Err(format!("'{}' is not a valid integer", v));
            }
            let n: i128 = match v.trim_start_matches('+').parse() {
                Ok(n) => n,
                Err(_) => return Err(format!("'{}' is out of range", v)),
            };
            match range {
                Some((lo, hi)) if n < lo || n > hi => Err(format!("'{}' is out of range", v)),
                _ => Ok(()),
            }
        }
        Prim::Float => ok(re!(r"^([+-]?(\d+(\.\d*)?|\.\d+)([eE][+-]?\d+)?|[+-]?INF|NaN)$").is_match(v)),
        Prim::Duration => ok(re!(r"^-?P(\d+Y)?(\d+M)?(\d+D)?(T(\d+H)?(\d+M)?(\d+(\.\d+)?S)?)?$").is_match(v) && v != "P" && !v.ends_with('T') && !v.starts_with("-P") || re!(r"^P(\d+Y)?(\d+M)?(\d+D)?(T(\d+H)?(\d+M)?(\d+(\.\d+)?S)?)?$").is_match(v) && v != "P" && !v.ends_with('T')),
        Prim::DateTime => {
            let c = re!(r"^(-?\d{4,})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(\.\d+)?(Z|[+-]\d{2}:\d{2})?$").captures(v).ok_or_else(|| format!("'{}' is not a valid dateTime", v))?;
            let (m, d, h, mi, s): (u32, u32, u32, u32, u32) = (c[2].parse().unwrap(), c[3].parse().unwrap(), c[4].parse().unwrap(), c[5].parse().unwrap(), c[6].parse().unwrap());
            ok(valid_date(&c[1], m, d) && (h < 24 || (h == 24 && mi == 0 && s == 0)) && mi < 60 && s < 60)
        }
        Prim::Date => {
            let c = re!(r"^(-?\d{4,})-(\d{2})-(\d{2})(Z|[+-]\d{2}:\d{2})?$").captures(v).ok_or_else(|| format!("'{}' is not a valid date", v))?;
            ok(valid_date(&c[1], c[2].parse().unwrap(), c[3].parse().unwrap()))
        }
        Prim::Time => {
            let c = re!(r"^(\d{2}):(\d{2}):(\d{2})(\.\d+)?(Z|[+-]\d{2}:\d{2})?$").captures(v).ok_or_else(|| format!("'{}' is not a valid time", v))?;
            let (h, m, s): (u32, u32, u32) = (c[1].parse().unwrap(), c[2].parse().unwrap(), c[3].parse().unwrap());
            ok((h < 24 || (h == 24 && m == 0 && s == 0)) && m < 60 && s < 60)
        }
        Prim::GYearMonth => {
            let c = re!(r"^(-?\d{4,})-(\d{2})(Z|[+-]\d{2}:\d{2})?$").captures(v).ok_or_else(|| format!("'{}' is not a valid gYearMonth", v))?;
            ok((1..=12).contains(&c[2].parse::<u32>().unwrap()))
        }
        Prim::GYear => ok(re!(r"^-?\d{4,}(Z|[+-]\d{2}:\d{2})?$").is_match(v)),
        Prim::GMonthDay => {
            let c = re!(r"^--(\d{2})-(\d{2})(Z|[+-]\d{2}:\d{2})?$").captures(v).ok_or_else(|| format!("'{}' is not a valid gMonthDay", v))?;
            ok(valid_date("2000", c[1].parse().unwrap(), c[2].parse().unwrap()))
        }
        Prim::GDay => {
            let c = re!(r"^---(\d{2})(Z|[+-]\d{2}:\d{2})?$").captures(v).ok_or_else(|| format!("'{}' is not a valid gDay", v))?;
            ok((1..=31).contains(&c[1].parse::<u32>().unwrap()))
        }
        Prim::GMonth => {
            let c = re!(r"^--(\d{2})(Z|[+-]\d{2}:\d{2})?$").captures(v).ok_or_else(|| format!("'{}' is not a valid gMonth", v))?;
            ok((1..=12).contains(&c[1].parse::<u32>().unwrap()))
        }
        Prim::HexBinary => ok(v.len() % 2 == 0 && v.bytes().all(|b| b.is_ascii_hexdigit())),
        Prim::Base64 => {
            let s: String = v.chars().filter(|c| !c.is_whitespace()).collect();
            ok(s.len() % 4 == 0 && re!(r"^[A-Za-z0-9+/]*={0,2}$").is_match(&s))
        }
    }
    .map(|_| {
        let _ = tz;
    })
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn apply_ws(w: Ws, s: &str) -> String {
    match w {
        Ws::Preserve => s.to_string(),
        Ws::Replace => s.chars().map(|c| if matches!(c, '\t' | '\n' | '\r') { ' ' } else { c }).collect(),
        Ws::Collapse => collapse(s),
    }
}

// --------------------------------------------------------------------------------------------------------- compiling

/// `prefix:local` in the scope of `node`.
fn ns_in_scope(doc: &Doc, node: usize, prefix: &str) -> Option<String> {
    if prefix == "xml" {
        return Some("http://www.w3.org/XML/1998/namespace".into());
    }
    let want = if prefix.is_empty() { "xmlns".to_string() } else { format!("xmlns:{}", prefix) };
    let mut cur = Some(node);
    while let Some(n) = cur {
        if let View::Elem { attrs, .. } = doc.view(n) {
            if let Some((_, v)) = attrs.iter().find(|(k, _)| *k == want) {
                return Some(v.clone());
            }
        }
        cur = doc.parent(n);
    }
    if prefix.is_empty() {
        Some(String::new())
    } else {
        None
    }
}

fn local_of(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn elem_name<'a>(doc: &'a Doc, id: usize) -> Option<&'a str> {
    match doc.view(id) {
        View::Elem { name, .. } => Some(local_of(name)),
        _ => None,
    }
}

fn attr<'a>(doc: &'a Doc, id: usize, name: &str) -> Option<&'a str> {
    match doc.view(id) {
        View::Elem { attrs, .. } => attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()),
        _ => None,
    }
}

fn element_children(doc: &Doc, id: usize) -> Vec<usize> {
    doc.kids(id).iter().copied().filter(|&k| matches!(doc.view(k), View::Elem { .. }) && elem_name(doc, k) != Some("annotation")).collect()
}

struct SchemaInfo {
    tns: String,
    elem_qualified: bool,
    attr_qualified: bool,
}

struct Builder<'a> {
    docs: &'a [Doc],
    info: Vec<SchemaInfo>,
    /// the raw global components: name -> (document, node)
    raw_types: HashMap<Name, (usize, usize)>,
    raw_groups: HashMap<Name, (usize, usize)>,
    raw_attr_groups: HashMap<Name, (usize, usize)>,
    raw_attrs: HashMap<Name, (usize, usize)>,
    built: RefCell<HashMap<Name, Option<Rc<Def>>>>,
    depth: Cell<usize>,
}

impl<'a> Builder<'a> {
    fn qname(&self, di: usize, node: usize, s: &str) -> Result<Name, String> {
        let doc = &self.docs[di];
        match s.split_once(':') {
            Some((p, l)) => Ok((ns_in_scope(doc, node, p).ok_or_else(|| format!("the prefix '{}' is not declared", p))?, l.to_string())),
            None => Ok((ns_in_scope(doc, node, "").unwrap_or_default(), s.to_string())),
        }
    }

    fn occurs(&self, di: usize, node: usize) -> Result<(u32, Option<u32>), String> {
        let doc = &self.docs[di];
        let min = match attr(doc, node, "minOccurs") {
            Some(v) => v.trim().parse::<u32>().map_err(|_| format!("minOccurs '{}' is not valid", v))?,
            None => 1,
        };
        let max = match attr(doc, node, "maxOccurs") {
            Some("unbounded") => None,
            Some(v) => Some(v.trim().parse::<u32>().map_err(|_| format!("maxOccurs '{}' is not valid", v))?),
            None => Some(1),
        };
        if max.is_some_and(|m| m < min) {
            return Err("maxOccurs is less than minOccurs".into());
        }
        Ok((min, max))
    }

    fn type_attr(&self, di: usize, node: usize, a: &str) -> Result<Option<TypeSpec>, String> {
        match attr(&self.docs[di], node, a) {
            Some(t) => Ok(Some(TypeSpec::Named(self.qname(di, node, t.trim())?))),
            None => Ok(None),
        }
    }

    /// A type given by a `type` attribute or by an inline `simpleType` / `complexType` child.
    fn type_of(&self, di: usize, node: usize) -> Result<Option<TypeSpec>, String> {
        if let Some(t) = self.type_attr(di, node, "type")? {
            return Ok(Some(t));
        }
        for k in element_children(&self.docs[di], node) {
            match elem_name(&self.docs[di], k) {
                Some("simpleType") => return Ok(Some(TypeSpec::Simple(Rc::new(self.simple(di, k)?)))),
                Some("complexType") => return Ok(Some(TypeSpec::Complex(Rc::new(self.complex(di, k)?)))),
                _ => {}
            }
        }
        Ok(None)
    }

    fn element(&self, di: usize, node: usize, global: bool) -> Result<Rc<Element>, String> {
        let doc = &self.docs[di];
        let name = attr(doc, node, "name").ok_or("an element needs a name or a ref")?;
        let info = &self.info[di];
        let qualified = global || match attr(doc, node, "form") {
            Some("qualified") => true,
            Some(_) => false,
            None => info.elem_qualified,
        };
        Ok(Rc::new(Element {
            name: (if qualified { info.tns.clone() } else { String::new() }, name.to_string()),
            ty: self.type_of(di, node)?,
            nillable: attr(doc, node, "nillable") == Some("true"),
            default: attr(doc, node, "default").map(|s| s.to_string()),
            fixed: attr(doc, node, "fixed").map(|s| s.to_string()),
        }))
    }

    fn wildcard(&self, di: usize, node: usize) -> Result<Wildcard, String> {
        let doc = &self.docs[di];
        let tns = &self.info[di].tns;
        let ns = attr(doc, node, "namespace").unwrap_or("##any");
        let process = match attr(doc, node, "processContents") {
            Some("lax") => Process::Lax,
            Some("skip") => Process::Skip,
            _ => Process::Strict,
        };
        let mut w = Wildcard { any: false, other_than: None, namespaces: vec![], process };
        match ns.trim() {
            "##any" => w.any = true,
            "##other" => w.other_than = Some(tns.clone()),
            list => {
                for t in list.split_whitespace() {
                    w.namespaces.push(match t {
                        "##local" => String::new(),
                        "##targetNamespace" => tns.clone(),
                        other => other.to_string(),
                    });
                }
            }
        }
        Ok(w)
    }

    fn particle(&self, di: usize, node: usize) -> Result<Option<Particle>, String> {
        let doc = &self.docs[di];
        let (min, max) = self.occurs(di, node)?;
        let kids = |me: &Self| -> Result<Vec<Particle>, String> {
            let mut v = vec![];
            for k in element_children(&me.docs[di], node) {
                if let Some(p) = me.particle(di, k)? {
                    v.push(p);
                }
            }
            Ok(v)
        };
        let term = match elem_name(doc, node) {
            Some("element") => match attr(doc, node, "ref") {
                Some(r) => Term::Ref(self.qname(di, node, r.trim())?),
                None => Term::Element(self.element(di, node, false)?),
            },
            Some("sequence") => Term::Seq(kids(self)?),
            Some("choice") => Term::Choice(kids(self)?),
            Some("all") => Term::All(kids(self)?),
            Some("any") => Term::Any(self.wildcard(di, node)?),
            Some("group") => {
                let r = attr(doc, node, "ref").ok_or("a group in a content model needs a ref")?;
                let name = self.qname(di, node, r.trim())?;
                let (gd, gn) = *self.raw_groups.get(&name).ok_or_else(|| format!("the group '{}' is not defined", name.1))?;
                if self.depth.get() > 40 {
                    return Err("groups refer to each other in a cycle".into());
                }
                self.depth.set(self.depth.get() + 1);
                let inner = element_children(&self.docs[gd], gn).into_iter().find_map(|k| self.particle(gd, k).transpose());
                self.depth.set(self.depth.get() - 1);
                let inner = inner.ok_or("an empty group")??;
                return Ok(Some(Particle { min, max, term: Term::Seq(vec![inner]) }));
            }
            _ => return Ok(None),
        };
        Ok(Some(Particle { min, max, term }))
    }

    fn attr_use(&self, di: usize, node: usize) -> Result<AttrUse, String> {
        let doc = &self.docs[di];
        let required = attr(doc, node, "use") == Some("required");
        let prohibited = attr(doc, node, "use") == Some("prohibited");
        if let Some(r) = attr(doc, node, "ref") {
            let name = self.qname(di, node, r.trim())?;
            let (gd, gn) = *self.raw_attrs.get(&name).ok_or_else(|| format!("the attribute '{}' is not declared", name.1))?;
            let mut u = self.attr_use(gd, gn)?;
            u.required = required;
            u.prohibited = prohibited;
            if let Some(d) = attr(doc, node, "default") {
                u.default = Some(d.to_string());
            }
            if let Some(d) = attr(doc, node, "fixed") {
                u.fixed = Some(d.to_string());
            }
            return Ok(u);
        }
        let n = attr(doc, node, "name").ok_or("an attribute needs a name or a ref")?;
        let info = &self.info[di];
        let global = doc.parent(node).and_then(|p| elem_name(doc, p)) == Some("schema");
        let qualified = global || match attr(doc, node, "form") {
            Some("qualified") => true,
            Some(_) => false,
            None => info.attr_qualified,
        };
        Ok(AttrUse {
            name: (if qualified { info.tns.clone() } else { String::new() }, n.to_string()),
            ty: self.type_of(di, node)?.unwrap_or_else(|| TypeSpec::Named((XS.into(), "anySimpleType".into()))),
            required,
            prohibited,
            default: attr(doc, node, "default").map(|s| s.to_string()),
            fixed: attr(doc, node, "fixed").map(|s| s.to_string()),
        })
    }

    /// The attributes, attribute groups and attribute wildcard among the children of a type definition.
    fn attributes(&self, di: usize, node: usize, attrs: &mut Vec<AttrUse>, any: &mut Option<Wildcard>) -> Result<(), String> {
        let doc = &self.docs[di];
        for k in element_children(doc, node) {
            match elem_name(doc, k) {
                Some("attribute") => {
                    let u = self.attr_use(di, k)?;
                    attrs.retain(|a| a.name != u.name);
                    attrs.push(u);
                }
                Some("attributeGroup") => {
                    let r = attr(doc, k, "ref").ok_or("an attributeGroup in a type needs a ref")?;
                    let name = self.qname(di, k, r.trim())?;
                    let (gd, gn) = *self.raw_attr_groups.get(&name).ok_or_else(|| format!("the attribute group '{}' is not defined", name.1))?;
                    if self.depth.get() > 40 {
                        return Err("attribute groups refer to each other in a cycle".into());
                    }
                    self.depth.set(self.depth.get() + 1);
                    let r = self.attributes(gd, gn, attrs, any);
                    self.depth.set(self.depth.get() - 1);
                    r?;
                }
                Some("anyAttribute") => *any = Some(self.wildcard(di, k)?),
                _ => {}
            }
        }
        Ok(())
    }

    fn facets(&self, di: usize, node: usize) -> Result<Facets, String> {
        let doc = &self.docs[di];
        let mut f = Facets::default();
        for k in element_children(doc, node) {
            let Some(name) = elem_name(doc, k) else { continue };
            let Some(v) = attr(doc, k, "value") else { continue };
            let num = |v: &str| v.trim().parse::<usize>().map_err(|_| format!("the value '{}' of {} is not a number", v, name));
            match name {
                "enumeration" => f.enumeration.push(v.to_string()),
                "pattern" => f.patterns.push(Regex::new(&format!("^(?:{})$", v)).map_err(|e| format!("the pattern '{}' is not supported: {}", v, e))?),
                "length" => f.length = Some(num(v)?),
                "minLength" => f.min_length = Some(num(v)?),
                "maxLength" => f.max_length = Some(num(v)?),
                "minInclusive" => f.min_inclusive = Some(v.to_string()),
                "maxInclusive" => f.max_inclusive = Some(v.to_string()),
                "minExclusive" => f.min_exclusive = Some(v.to_string()),
                "maxExclusive" => f.max_exclusive = Some(v.to_string()),
                "totalDigits" => f.total_digits = Some(num(v)?),
                "fractionDigits" => f.fraction_digits = Some(num(v)?),
                "whiteSpace" => {
                    f.whitespace = Some(match v {
                        "preserve" => Ws::Preserve,
                        "replace" => Ws::Replace,
                        "collapse" => Ws::Collapse,
                        _ => return Err(format!("whiteSpace '{}' is not valid", v)),
                    })
                }
                _ => {}
            }
        }
        Ok(f)
    }

    fn simple(&self, di: usize, node: usize) -> Result<SimpleType, String> {
        let doc = &self.docs[di];
        for k in element_children(doc, node) {
            match elem_name(doc, k) {
                Some("restriction") => {
                    let base = self.type_of_base(di, k)?;
                    return Ok(SimpleType { kind: SimpleKind::Restriction { base, facets: self.facets(di, k)? } });
                }
                Some("list") => {
                    let item = self.type_of_attr_or_inline(di, k, "itemType")?;
                    return Ok(SimpleType { kind: SimpleKind::List(item) });
                }
                Some("union") => {
                    let mut members = vec![];
                    if let Some(m) = attr(doc, k, "memberTypes") {
                        for t in m.split_whitespace() {
                            members.push(TypeSpec::Named(self.qname(di, k, t)?));
                        }
                    }
                    for c in element_children(doc, k) {
                        if elem_name(doc, c) == Some("simpleType") {
                            members.push(TypeSpec::Simple(Rc::new(self.simple(di, c)?)));
                        }
                    }
                    return Ok(SimpleType { kind: SimpleKind::Union(members) });
                }
                _ => {}
            }
        }
        Err("a simpleType needs a restriction, list or union".into())
    }

    fn type_of_attr_or_inline(&self, di: usize, node: usize, a: &str) -> Result<TypeSpec, String> {
        if let Some(t) = self.type_attr(di, node, a)? {
            return Ok(t);
        }
        for k in element_children(&self.docs[di], node) {
            if elem_name(&self.docs[di], k) == Some("simpleType") {
                return Ok(TypeSpec::Simple(Rc::new(self.simple(di, k)?)));
            }
        }
        Err(format!("no {} or inline simpleType", a))
    }

    fn type_of_base(&self, di: usize, node: usize) -> Result<TypeSpec, String> {
        self.type_of_attr_or_inline(di, node, "base")
    }

    /// A named complex type, built on demand (a base type may come later in the document).
    fn complex_named(&self, name: &Name) -> Result<Option<Rc<ComplexType>>, String> {
        if name.0 == XS {
            return Ok(None);
        }
        let Some(&(di, node)) = self.raw_types.get(name) else { return Ok(None) };
        if elem_name(&self.docs[di], node) != Some("complexType") {
            return Ok(None);
        }
        if self.depth.get() > 40 {
            return Err(format!("the type '{}' is derived from itself", name.1));
        }
        self.depth.set(self.depth.get() + 1);
        let r = self.complex(di, node);
        self.depth.set(self.depth.get() - 1);
        r.map(|c| Some(Rc::new(c)))
    }

    fn complex(&self, di: usize, node: usize) -> Result<ComplexType, String> {
        let doc = &self.docs[di];
        let mut mixed = attr(doc, node, "mixed") == Some("true");
        let mut attrs = vec![];
        let mut any = None;
        let mut content = Content::Empty;
        for k in element_children(doc, node) {
            match elem_name(doc, k) {
                Some("sequence" | "choice" | "all" | "group" | "element" | "any") => {
                    if let Some(p) = self.particle(di, k)? {
                        content = Content::Particle(p);
                    }
                }
                Some("simpleContent") => {
                    let d = element_children(doc, k).into_iter().find(|&c| matches!(elem_name(doc, c), Some("extension" | "restriction"))).ok_or("simpleContent needs an extension or a restriction")?;
                    let base = self.qname(di, d, attr(doc, d, "base").ok_or("an extension or restriction needs a base")?.trim())?;
                    let extension = elem_name(doc, d) == Some("extension");
                    // the value type: that of a simple base, or the value type of the base complex type
                    let base_complex = self.complex_named(&base)?;
                    let value = match &base_complex {
                        Some(c) => {
                            attrs.extend(c.attrs.iter().cloned());
                            any = c.any_attr.clone();
                            match &c.content {
                                Content::Simple(t) => t.clone(),
                                _ => return Err("the base of a simpleContent must have simple content".into()),
                            }
                        }
                        None => TypeSpec::Named(base.clone()),
                    };
                    content = Content::Simple(if extension {
                        value
                    } else {
                        // restriction: facets, and possibly a new inline simple type as the base
                        TypeSpec::Simple(Rc::new(SimpleType { kind: SimpleKind::Restriction { base: value, facets: self.facets(di, d)? } }))
                    });
                    self.attributes(di, d, &mut attrs, &mut any)?;
                }
                Some("complexContent") => {
                    if attr(doc, k, "mixed") == Some("true") {
                        mixed = true;
                    }
                    let d = element_children(doc, k).into_iter().find(|&c| matches!(elem_name(doc, c), Some("extension" | "restriction"))).ok_or("complexContent needs an extension or a restriction")?;
                    let base = self.qname(di, d, attr(doc, d, "base").ok_or("an extension or restriction needs a base")?.trim())?;
                    let extension = elem_name(doc, d) == Some("extension");
                    let base_complex = self.complex_named(&base)?;
                    let mut own = None;
                    for c in element_children(doc, d) {
                        if matches!(elem_name(doc, c), Some("sequence" | "choice" | "all" | "group")) {
                            own = self.particle(di, c)?;
                        }
                    }
                    let base_attrs: Vec<AttrUse> = base_complex.as_ref().map(|c| c.attrs.clone()).unwrap_or_default();
                    attrs = base_attrs;
                    any = base_complex.as_ref().and_then(|c| c.any_attr.clone());
                    content = match (extension, base_complex.as_ref().map(|c| c.content.clone()), own) {
                        (true, Some(Content::Particle(b)), Some(o)) => Content::Particle(Particle { min: 1, max: Some(1), term: Term::Seq(vec![b, o]) }),
                        (true, Some(Content::Particle(b)), None) => Content::Particle(b),
                        (true, Some(Content::Simple(t)), _) => Content::Simple(t),
                        (true, _, Some(o)) => Content::Particle(o),
                        (false, _, Some(o)) => Content::Particle(o),
                        _ => Content::Empty,
                    };
                    if base_complex.as_ref().is_some_and(|c| c.mixed) {
                        mixed = true;
                    }
                    if base.0 == XS && base.1 == "anyType" && !extension {
                        // restriction of anyType: the content is whatever is written
                    }
                    self.attributes(di, d, &mut attrs, &mut any)?;
                }
                _ => {}
            }
        }
        self.attributes(di, node, &mut attrs, &mut any)?;
        Ok(ComplexType { mixed, content, attrs, any_attr: any })
    }
}

/// The namespace of each `<xs:schema>` and its defaults.
fn schema_info(doc: &Doc, root: usize) -> SchemaInfo {
    SchemaInfo { tns: attr(doc, root, "targetNamespace").unwrap_or("").to_string(), elem_qualified: attr(doc, root, "elementFormDefault") == Some("qualified"), attr_qualified: attr(doc, root, "attributeFormDefault") == Some("qualified") }
}

/// The target namespace of a schema document (empty for none); `None` if it is not a schema.
pub fn target_namespace(text: &str) -> Option<String> {
    let doc = Doc::parse(text)?;
    let root = doc.kids(0).iter().copied().find(|&k| matches!(doc.view(k), View::Elem { .. }))?;
    Some(attr(&doc, root, "targetNamespace").unwrap_or("").to_string())
}

impl Collection {
    /// Compile the schema documents of a collection (each text may hold one `<xs:schema>`).
    pub fn compile(texts: &[String]) -> Result<Collection, String> {
        let mut docs = vec![];
        for t in texts {
            docs.push(Doc::parse(t).ok_or("a schema is not well-formed XML")?);
        }
        let mut info = vec![];
        let mut roots = vec![];
        for d in &docs {
            let root = d.kids(0).iter().copied().find(|&k| matches!(d.view(k), View::Elem { .. })).ok_or("a schema document is empty")?;
            if elem_name(d, root) != Some("schema") {
                return Err(format!("the root of a schema must be 'schema', not '{}'", elem_name(d, root).unwrap_or("")));
            }
            info.push(schema_info(d, root));
            roots.push(root);
        }
        let mut b = Builder { docs: &docs, info, raw_types: HashMap::new(), raw_groups: HashMap::new(), raw_attr_groups: HashMap::new(), raw_attrs: HashMap::new(), built: RefCell::new(HashMap::new()), depth: Cell::new(0) };
        let mut raw_elements: Vec<(usize, usize)> = vec![];
        let mut namespaces: Vec<String> = vec![];
        for (di, &root) in roots.iter().enumerate() {
            let tns = b.info[di].tns.clone();
            if !namespaces.contains(&tns) {
                namespaces.push(tns.clone());
            }
            for k in element_children(&docs[di], root) {
                let Some(kind) = elem_name(&docs[di], k) else { continue };
                let Some(n) = attr(&docs[di], k, "name") else { continue };
                let key = (tns.clone(), n.to_string());
                let dup = |what: &str| format!("the {} '{}' is defined twice", what, n);
                match kind {
                    "complexType" | "simpleType" => {
                        if b.raw_types.insert(key, (di, k)).is_some() {
                            return Err(dup("type"));
                        }
                    }
                    "group" => {
                        b.raw_groups.insert(key, (di, k));
                    }
                    "attributeGroup" => {
                        b.raw_attr_groups.insert(key, (di, k));
                    }
                    "attribute" => {
                        b.raw_attrs.insert(key, (di, k));
                    }
                    "element" => raw_elements.push((di, k)),
                    _ => {}
                }
            }
        }
        let mut types = HashMap::new();
        let names: Vec<(Name, (usize, usize))> = b.raw_types.iter().map(|(n, p)| (n.clone(), *p)).collect();
        for (name, (di, node)) in names {
            let def = if elem_name(&docs[di], node) == Some("simpleType") {
                Def::Simple(Rc::new(b.simple(di, node).map_err(|e| format!("type '{}': {}", name.1, e))?))
            } else {
                Def::Complex(b.complex_named(&name).map_err(|e| format!("type '{}': {}", name.1, e))?.ok_or("complex type")?)
            };
            types.insert(name, def);
        }
        let mut elements = HashMap::new();
        for (di, node) in raw_elements {
            let e = b.element(di, node, true).map_err(|e| format!("element: {}", e))?;
            if elements.insert(e.name.clone(), e.clone()).is_some() {
                return Err(format!("the element '{}' is declared twice", e.name.1));
            }
        }
        let mut attributes = HashMap::new();
        let anames: Vec<(Name, (usize, usize))> = b.raw_attrs.iter().map(|(n, p)| (n.clone(), *p)).collect();
        for (name, (di, node)) in anames {
            attributes.insert(name, b.attr_use(di, node)?);
        }
        let col = Collection { elements, types, attributes, namespaces };
        col.check_references()?;
        Ok(col)
    }

    /// The namespaces of the collection (the target namespaces of its schemas; empty for none).
    pub fn namespaces(&self) -> &[String] {
        &self.namespaces
    }

    /// Every named type that a declaration refers to must exist.
    fn check_references(&self) -> Result<(), String> {
        let mut missing: Option<String> = None;
        let mut check = |ts: &TypeSpec| {
            if let TypeSpec::Named(n) = ts {
                // an element reference is passed on as a pseudo type name "#ref" with the name as "namespace|local"
                if n.0 == "#ref" {
                    let (ns, local) = n.1.split_once('|').unwrap_or(("", ""));
                    if !self.elements.contains_key(&(ns.to_string(), local.to_string())) && missing.is_none() {
                        missing = Some(format!("the element '{}' is not declared", local));
                    }
                } else if self.lookup(n).is_none() && missing.is_none() {
                    missing = Some(format!("the type '{}' is not defined", n.1));
                }
            }
        };
        fn walk_simple(c: &Collection, st: &SimpleType, check: &mut dyn FnMut(&TypeSpec)) {
            match &st.kind {
                SimpleKind::Restriction { base, .. } => visit(c, base, check),
                SimpleKind::List(i) => visit(c, i, check),
                SimpleKind::Union(m) => m.iter().for_each(|t| visit(c, t, check)),
            }
        }
        fn visit(c: &Collection, ts: &TypeSpec, check: &mut dyn FnMut(&TypeSpec)) {
            check(ts);
            match ts {
                TypeSpec::Simple(s) => walk_simple(c, s, check),
                TypeSpec::Complex(ct) => walk_complex(c, ct, check),
                TypeSpec::Named(_) => {}
            }
        }
        fn walk_particle(c: &Collection, p: &Particle, check: &mut dyn FnMut(&TypeSpec)) {
            match &p.term {
                Term::Element(e) => {
                    if let Some(t) = &e.ty {
                        visit(c, t, check)
                    }
                }
                Term::Ref(n) => check(&TypeSpec::Named(("#ref".into(), format!("{}|{}", n.0, n.1)))),
                Term::Seq(v) | Term::Choice(v) | Term::All(v) => v.iter().for_each(|x| walk_particle(c, x, check)),
                _ => {}
            }
        }
        fn walk_complex(c: &Collection, ct: &ComplexType, check: &mut dyn FnMut(&TypeSpec)) {
            ct.attrs.iter().for_each(|a| visit(c, &a.ty, check));
            match &ct.content {
                Content::Simple(t) => visit(c, t, check),
                Content::Particle(p) => walk_particle(c, p, check),
                Content::Empty => {}
            }
        }
        for e in self.elements.values() {
            if let Some(t) = &e.ty {
                visit(self, t, &mut check);
            }
        }
        for d in self.types.values() {
            match d {
                Def::Simple(s) => walk_simple(self, s, &mut check),
                Def::Complex(c) => walk_complex(self, c, &mut check),
            }
        }
        for a in self.attributes.values() {
            visit(self, &a.ty, &mut check);
        }
        match missing {
            Some(m) => Err(m),
            None => Ok(()),
        }
    }

    fn lookup(&self, n: &Name) -> Option<()> {
        if n.0 == XS {
            return if n.1 == "anyType" || builtin(&n.1).is_some() { Some(()) } else { None };
        }
        self.types.get(n).map(|_| ())
    }
}

// ----------------------------------------------------------------------------------------------------- validation

enum Resolved {
    Any,
    Simple(TypeSpec),
    Complex(Rc<ComplexType>),
}

fn show_name(n: &Name) -> String {
    if n.0.is_empty() {
        n.1.clone()
    } else {
        format!("{{{}}}{}", n.0, n.1)
    }
}

#[derive(Clone)]
enum Slot {
    Decl(Rc<Element>),
    Wild(Process),
}

struct Kid {
    id: usize,
    name: Name,
}

struct Matcher<'a> {
    col: &'a Collection,
    kids: Vec<Kid>,
    far: Cell<usize>,
    expected: RefCell<Vec<String>>,
}

type Ways = Vec<(usize, Vec<Slot>)>;

impl<'a> Matcher<'a> {
    fn push_unique(out: &mut Ways, end: usize, a: Vec<Slot>) {
        if !out.iter().any(|x| x.0 == end) {
            out.push((end, a));
        }
    }

    fn note(&self, pos: usize, name: &str) {
        if pos > self.far.get() {
            self.far.set(pos);
            self.expected.borrow_mut().clear();
        }
        if pos == self.far.get() {
            let mut e = self.expected.borrow_mut();
            let q = format!("'{}'", name);
            if !e.contains(&q) {
                e.push(q);
            }
        }
    }

    fn wild_ok(&self, w: &Wildcard, ns: &str) -> bool {
        w.any || w.namespaces.iter().any(|n| n == ns) || w.other_than.as_ref().is_some_and(|t| t != ns && !ns.is_empty())
    }

    /// Every way the particle (with its occurrences) can match the children from `pos`.
    fn rep(&self, p: &Particle, pos: usize) -> Ways {
        let mut out: Ways = vec![];
        let mut level: Ways = vec![(pos, vec![])];
        let max = p.max.map(|m| m as usize).unwrap_or(usize::MAX);
        let cap = self.kids.len() + p.min as usize + 1;
        if p.min == 0 {
            out.push((pos, vec![]));
        }
        for count in 1..=max.min(cap) {
            let mut next: Ways = vec![];
            for (e, a) in &level {
                for (e2, a2) in self.once(&p.term, *e) {
                    if e2 == *e && count > p.min as usize {
                        continue;
                    }
                    let mut aa = a.clone();
                    aa.extend(a2);
                    Self::push_unique(&mut next, e2, aa);
                }
            }
            if next.is_empty() {
                break;
            }
            if count >= p.min as usize {
                for n in &next {
                    Self::push_unique(&mut out, n.0, n.1.clone());
                }
            }
            level = next;
        }
        out
    }

    fn seq(&self, ps: &[Particle], pos: usize) -> Ways {
        let mut states: Ways = vec![(pos, vec![])];
        for p in ps {
            let mut next: Ways = vec![];
            for (e, a) in &states {
                for (e2, a2) in self.rep(p, *e) {
                    let mut aa = a.clone();
                    aa.extend(a2);
                    Self::push_unique(&mut next, e2, aa);
                }
            }
            if next.is_empty() {
                return vec![];
            }
            states = next;
        }
        states
    }

    fn all(&self, ps: &[Particle], pos: usize, used: u32, acc: &[Slot], out: &mut Ways) {
        let done = ps.iter().enumerate().all(|(i, p)| used & (1 << i) != 0 || p.min == 0);
        if done {
            Self::push_unique(out, pos, acc.to_vec());
        }
        for (i, p) in ps.iter().enumerate() {
            if used & (1 << i) != 0 {
                continue;
            }
            for (e, a) in self.rep(p, pos) {
                if e > pos {
                    let mut aa = acc.to_vec();
                    aa.extend(a);
                    self.all(ps, e, used | (1 << i), &aa, out);
                }
            }
        }
    }

    fn once(&self, t: &Term, pos: usize) -> Ways {
        match t {
            Term::Element(e) => {
                self.note(pos, &e.name.1);
                match self.kids.get(pos) {
                    Some(k) if k.name == e.name => vec![(pos + 1, vec![Slot::Decl(e.clone())])],
                    _ => vec![],
                }
            }
            Term::Ref(n) => {
                self.note(pos, &n.1);
                match (self.kids.get(pos), self.col.elements.get(n)) {
                    (Some(k), Some(e)) if k.name == *n => vec![(pos + 1, vec![Slot::Decl(e.clone())])],
                    _ => vec![],
                }
            }
            Term::Seq(ps) => self.seq(ps, pos),
            Term::Choice(ps) => {
                let mut out: Ways = vec![];
                for p in ps {
                    for (e, a) in self.rep(p, pos) {
                        Self::push_unique(&mut out, e, a);
                    }
                }
                out
            }
            Term::All(ps) => {
                let mut out: Ways = vec![];
                if ps.len() <= 31 {
                    self.all(ps, pos, 0, &[], &mut out);
                }
                out
            }
            Term::Any(w) => {
                self.note(pos, "*");
                match self.kids.get(pos) {
                    Some(k) if self.wild_ok(w, &k.name.0) => vec![(pos + 1, vec![Slot::Wild(w.process)])],
                    _ => vec![],
                }
            }
        }
    }
}

struct Validator<'a> {
    col: &'a Collection,
    doc: &'a Doc,
}

impl Collection {
    /// Validate a parsed document. With `document`, there must be one root element (the DOCUMENT facet of `xml(DOCUMENT ...)`);
    /// otherwise any number of elements and text (the CONTENT facet).
    pub fn validate(&self, doc: &Doc, document: bool) -> Result<(), XsdError> {
        let v = Validator { col: self, doc };
        let kids: Vec<usize> = doc.kids(0).to_vec();
        let elements: Vec<usize> = kids.iter().copied().filter(|&k| matches!(doc.view(k), View::Elem { .. })).collect();
        if document {
            if elements.len() != 1 {
                return Err(verr(6965, format!("XML Validation: Invalid content. A DOCUMENT must have exactly one root element, found {}.", elements.len())));
            }
            for &k in &kids {
                if let View::Text(t) = doc.view(k) {
                    if !t.trim().is_empty() {
                        return Err(verr(6965, "XML Validation: Invalid content. Text is not allowed outside the root element of a DOCUMENT.".into()));
                    }
                }
            }
        }
        for (i, &e) in elements.iter().enumerate() {
            let name = v.name_of(e);
            let decl = self.elements.get(&name).ok_or_else(|| verr(6913, format!("XML Validation: Declaration not found for element '{}'. Location: /*:{}[{}].", show_name(&name), name.1, i + 1)))?;
            v.element(e, decl, &format!("/*:{}[{}]", name.1, i + 1))?;
        }
        Ok(())
    }

    fn resolve(&self, ts: &TypeSpec) -> Result<Resolved, String> {
        Ok(match ts {
            TypeSpec::Simple(_) => Resolved::Simple(ts.clone()),
            TypeSpec::Complex(c) => Resolved::Complex(c.clone()),
            TypeSpec::Named(n) => {
                if n.0 == XS {
                    if n.1 == "anyType" {
                        Resolved::Any
                    } else if builtin(&n.1).is_some() {
                        Resolved::Simple(ts.clone())
                    } else {
                        return Err(format!("the type '{}' is not defined", n.1));
                    }
                } else {
                    match self.types.get(n) {
                        Some(Def::Simple(s)) => Resolved::Simple(TypeSpec::Simple(s.clone())),
                        Some(Def::Complex(c)) => Resolved::Complex(c.clone()),
                        None => return Err(format!("the type '{}' is not defined", n.1)),
                    }
                }
            }
        })
    }

    fn prim_of(&self, ts: &TypeSpec) -> Option<(Prim, Option<(i128, i128)>)> {
        match self.resolve(ts).ok()? {
            Resolved::Simple(TypeSpec::Named(n)) => builtin(&n.1),
            Resolved::Simple(TypeSpec::Simple(s)) => match &s.kind {
                SimpleKind::Restriction { base, .. } => self.prim_of(base),
                _ => None,
            },
            _ => None,
        }
    }

    fn is_list(&self, ts: &TypeSpec) -> bool {
        match self.resolve(ts) {
            Ok(Resolved::Simple(TypeSpec::Simple(s))) => match &s.kind {
                SimpleKind::List(_) => true,
                SimpleKind::Restriction { base, .. } => self.is_list(base),
                SimpleKind::Union(_) => false,
            },
            _ => false,
        }
    }

    fn ws_of(&self, ts: &TypeSpec) -> Ws {
        match self.resolve(ts) {
            Ok(Resolved::Simple(TypeSpec::Named(n))) => match builtin(&n.1) {
                Some((Prim::Str, _)) => Ws::Preserve,
                Some((Prim::NormalizedString, _)) => Ws::Replace,
                _ => Ws::Collapse,
            },
            Ok(Resolved::Simple(TypeSpec::Simple(s))) => match &s.kind {
                SimpleKind::Restriction { base, facets } => facets.whitespace.unwrap_or_else(|| self.ws_of(base)),
                _ => Ws::Collapse,
            },
            _ => Ws::Preserve,
        }
    }

    /// Check a lexical value against a simple type; the result is the value after whitespace processing.
    fn simple_value(&self, ts: &TypeSpec, text: &str) -> Result<String, String> {
        let norm = apply_ws(self.ws_of(ts), text);
        match self.resolve(ts)? {
            Resolved::Any | Resolved::Complex(_) => Ok(norm),
            Resolved::Simple(TypeSpec::Named(n)) => {
                let (p, range) = builtin(&n.1).ok_or("unknown type")?;
                lexical(p, range, &norm)?;
                Ok(norm)
            }
            Resolved::Simple(TypeSpec::Simple(st)) => match &st.kind {
                SimpleKind::Restriction { base, facets } => {
                    let v = self.simple_value(base, &norm)?;
                    self.apply_facets(base, facets, &v)?;
                    Ok(v)
                }
                SimpleKind::List(item) => {
                    for part in norm.split_whitespace() {
                        self.simple_value(item, part)?;
                    }
                    Ok(norm)
                }
                SimpleKind::Union(members) => {
                    for m in members {
                        if self.simple_value(m, &norm).is_ok() {
                            return Ok(norm);
                        }
                    }
                    Err(format!("'{}' does not match any of the types of the union", norm))
                }
            },
            Resolved::Simple(TypeSpec::Complex(_)) => Ok(norm),
        }
    }

    fn apply_facets(&self, base: &TypeSpec, f: &Facets, v: &str) -> Result<(), String> {
        let prim = self.prim_of(base).map(|p| p.0);
        let list = self.is_list(base);
        let len = if list {
            v.split_whitespace().count()
        } else {
            match prim {
                Some(Prim::HexBinary) => v.len() / 2,
                Some(Prim::Base64) => v.chars().filter(|c| !c.is_whitespace() && *c != '=').count() * 3 / 4,
                _ => v.chars().count(),
            }
        };
        if let Some(n) = f.length {
            if len != n {
                return Err(format!("'{}' must have length {}", v, n));
            }
        }
        if f.min_length.is_some_and(|n| len < n) {
            return Err(format!("'{}' is shorter than {}", v, f.min_length.unwrap()));
        }
        if f.max_length.is_some_and(|n| len > n) {
            return Err(format!("'{}' is longer than {}", v, f.max_length.unwrap()));
        }
        if !f.patterns.is_empty() && !f.patterns.iter().any(|r| r.is_match(v)) {
            return Err(format!("'{}' does not match the pattern", v));
        }
        let numeric = matches!(prim, Some(Prim::Decimal | Prim::Integer | Prim::Float));
        if !f.enumeration.is_empty() {
            let same = |a: &str, b: &str| -> bool {
                if numeric {
                    match (a.trim().trim_start_matches('+').parse::<f64>(), b.trim().trim_start_matches('+').parse::<f64>()) {
                        (Ok(x), Ok(y)) => return x == y,
                        _ => {}
                    }
                }
                if prim == Some(Prim::Boolean) {
                    let t = |s: &str| s == "true" || s == "1";
                    return t(a) == t(b);
                }
                a == b
            };
            let cmp_ws = apply_ws(self.ws_of(base), "");
            let _ = cmp_ws;
            if !f.enumeration.iter().any(|e| same(&apply_ws(Ws::Collapse, e).as_str(), v) || same(e, v)) {
                return Err(format!("'{}' is not one of the allowed values", v));
            }
        }
        // ranges: numbers compare as numbers, dates and times by their text (a rough order that is right within one format)
        let cmp = |a: &str, b: &str| -> Option<std::cmp::Ordering> {
            if numeric {
                a.trim().trim_start_matches('+').parse::<f64>().ok()?.partial_cmp(&b.trim().trim_start_matches('+').parse::<f64>().ok()?)
            } else if matches!(prim, Some(Prim::DateTime | Prim::Date | Prim::Time | Prim::GYear | Prim::GYearMonth)) {
                let strip = |s: &str| s.trim_end_matches('Z').to_string();
                Some(strip(a).cmp(&strip(b)))
            } else {
                None
            }
        };
        use std::cmp::Ordering::*;
        if let Some(m) = &f.min_inclusive {
            if cmp(v, m) == Some(Less) {
                return Err(format!("'{}' is less than {}", v, m));
            }
        }
        if let Some(m) = &f.max_inclusive {
            if cmp(v, m) == Some(Greater) {
                return Err(format!("'{}' is greater than {}", v, m));
            }
        }
        if let Some(m) = &f.min_exclusive {
            if matches!(cmp(v, m), Some(Less | Equal)) {
                return Err(format!("'{}' is not greater than {}", v, m));
            }
        }
        if let Some(m) = &f.max_exclusive {
            if matches!(cmp(v, m), Some(Greater | Equal)) {
                return Err(format!("'{}' is not less than {}", v, m));
            }
        }
        if (f.total_digits.is_some() || f.fraction_digits.is_some()) && numeric {
            let t = v.trim_start_matches(['+', '-']);
            let (int, frac) = t.split_once('.').unwrap_or((t, ""));
            let frac = frac.trim_end_matches('0');
            let digits = (int.trim_start_matches('0').len() + frac.len()).max(1);
            if f.total_digits.is_some_and(|n| digits > n) {
                return Err(format!("'{}' has more than {} digits", v, f.total_digits.unwrap()));
            }
            if f.fraction_digits.is_some_and(|n| frac.len() > n) {
                return Err(format!("'{}' has more than {} fraction digits", v, f.fraction_digits.unwrap()));
            }
        }
        Ok(())
    }
}

impl<'a> Validator<'a> {
    fn name_of(&self, id: usize) -> Name {
        match self.doc.view(id) {
            View::Elem { name, .. } => match name.split_once(':') {
                Some((p, l)) => (ns_in_scope(self.doc, id, p).unwrap_or_default(), l.to_string()),
                None => (ns_in_scope(self.doc, id, "").unwrap_or_default(), name.to_string()),
            },
            _ => (String::new(), String::new()),
        }
    }

    fn text_of(&self, id: usize) -> String {
        self.doc.kids(id).iter().filter_map(|&k| if let View::Text(t) = self.doc.view(k) { Some(t) } else { None }).collect()
    }

    fn element_kids(&self, id: usize) -> Vec<Kid> {
        self.doc.kids(id).iter().copied().filter(|&k| matches!(self.doc.view(k), View::Elem { .. })).map(|k| Kid { id: k, name: self.name_of(k) }).collect()
    }

    fn element(&self, id: usize, decl: &Element, loc: &str) -> Result<(), XsdError> {
        let (attrs, _) = match self.doc.view(id) {
            View::Elem { attrs, name } => (attrs, name),
            _ => return Ok(()),
        };
        let xsi = |n: &str| -> Option<&str> { attrs.iter().find(|(k, _)| k.split_once(':').is_some_and(|(p, l)| l == n && ns_in_scope(self.doc, id, p).as_deref() == Some(XSI))).map(|(_, v)| v.as_str()) };
        let nil = xsi("nil").map(|v| v.trim() == "true" || v.trim() == "1").unwrap_or(false);
        // the type: xsi:type, else the declaration's, else anyType
        let mut ty = decl.ty.clone().unwrap_or_else(|| TypeSpec::Named((XS.into(), "anyType".into())));
        if let Some(t) = xsi("type") {
            let n = match t.split_once(':') {
                Some((p, l)) => (ns_in_scope(self.doc, id, p).unwrap_or_default(), l.to_string()),
                None => (ns_in_scope(self.doc, id, "").unwrap_or_default(), t.to_string()),
            };
            ty = TypeSpec::Named(n);
        }
        let resolved = self.col.resolve(&ty).map_err(|m| verr(6913, format!("XML Validation: {}. Location: {}.", m, loc)))?;
        if nil {
            if !decl.nillable {
                return Err(verr(6910, format!("XML Validation: The element '{}' is not nillable. Location: {}.", decl.name.1, loc)));
            }
            if !self.doc.kids(id).is_empty() {
                return Err(verr(6910, format!("XML Validation: The nil element '{}' must be empty. Location: {}.", decl.name.1, loc)));
            }
            return Ok(());
        }
        match resolved {
            Resolved::Simple(st) => {
                self.attributes(id, &[], None, loc)?;
                if !self.element_kids(id).is_empty() {
                    return Err(verr(6965, format!("XML Validation: Invalid content. Element '{}' has a simple type and cannot have child elements. Location: {}.", decl.name.1, loc)));
                }
                let mut text = self.text_of(id);
                if text.is_empty() {
                    if let Some(d) = decl.fixed.as_ref().or(decl.default.as_ref()) {
                        text = d.clone();
                    }
                }
                let v = self.col.simple_value(&st, &text).map_err(|m| verr(6926, format!("XML Validation: Invalid simple type value: {}. Location: {}.", m, loc)))?;
                if let Some(f) = &decl.fixed {
                    let fv = self.col.simple_value(&st, f).unwrap_or_default();
                    if fv != v {
                        return Err(verr(6926, format!("XML Validation: The value '{}' of element '{}' is not the fixed value '{}'. Location: {}.", v, decl.name.1, fv, loc)));
                    }
                }
                Ok(())
            }
            Resolved::Any => {
                self.attributes(id, &[], Some(&Wildcard { any: true, other_than: None, namespaces: vec![], process: Process::Lax }), loc)?;
                self.lax_children(id, loc)
            }
            Resolved::Complex(ct) => {
                self.attributes(id, &ct.attrs, ct.any_attr.as_ref(), loc)?;
                match &ct.content {
                    Content::Empty => {
                        if !self.element_kids(id).is_empty() || (!ct.mixed && !self.text_of(id).trim().is_empty()) {
                            return Err(verr(6965, format!("XML Validation: Invalid content. Element '{}' must be empty. Location: {}.", decl.name.1, loc)));
                        }
                        Ok(())
                    }
                    Content::Simple(vt) => {
                        if !self.element_kids(id).is_empty() {
                            return Err(verr(6965, format!("XML Validation: Invalid content. Element '{}' cannot have child elements. Location: {}.", decl.name.1, loc)));
                        }
                        let mut text = self.text_of(id);
                        if text.is_empty() {
                            if let Some(d) = decl.fixed.as_ref().or(decl.default.as_ref()) {
                                text = d.clone();
                            }
                        }
                        self.col.simple_value(vt, &text).map(|_| ()).map_err(|m| verr(6926, format!("XML Validation: Invalid simple type value: {}. Location: {}.", m, loc)))
                    }
                    Content::Particle(p) => {
                        if !ct.mixed && !self.text_of(id).trim().is_empty() {
                            return Err(verr(6965, format!("XML Validation: Invalid content. Element '{}' cannot have text. Location: {}.", decl.name.1, loc)));
                        }
                        let m = Matcher { col: self.col, kids: self.element_kids(id), far: Cell::new(0), expected: RefCell::new(vec![]) };
                        let ways = m.rep(p, 0);
                        let n = m.kids.len();
                        let Some((_, slots)) = ways.into_iter().find(|w| w.0 == n) else {
                            let far = m.far.get();
                            let exp = m.expected.borrow().join(",");
                            let msg = match m.kids.get(far) {
                                Some(k) => format!("XML Validation: Invalid content. Expected element(s): {} where element '{}' was specified. Location: {}.", if exp.is_empty() { "none".to_string() } else { exp }, show_name(&k.name), loc),
                                None => format!("XML Validation: Invalid content. Missing element(s). Expected element(s): {}. Location: {}.", exp, loc),
                            };
                            return Err(verr(6965, msg));
                        };
                        let mut counts: HashMap<String, usize> = HashMap::new();
                        for (k, slot) in m.kids.iter().zip(slots) {
                            let c = counts.entry(k.name.1.clone()).or_insert(0);
                            *c += 1;
                            let kloc = format!("{}/*:{}[{}]", loc, k.name.1, c);
                            self.slot(k, &slot, &kloc)?;
                        }
                        Ok(())
                    }
                }
            }
        }
    }

    fn slot(&self, k: &Kid, slot: &Slot, loc: &str) -> Result<(), XsdError> {
        match slot {
            Slot::Decl(d) => self.element(k.id, d, loc),
            Slot::Wild(Process::Skip) => Ok(()),
            Slot::Wild(p) => match self.col.elements.get(&k.name) {
                Some(d) => self.element(k.id, d, loc),
                None if *p == Process::Strict => Err(verr(6913, format!("XML Validation: Declaration not found for element '{}'. Location: {}.", show_name(&k.name), loc))),
                None => self.lax_children(k.id, loc),
            },
        }
    }

    /// Children of anyType content: validated where a global declaration exists, otherwise accepted.
    fn lax_children(&self, id: usize, loc: &str) -> Result<(), XsdError> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for k in self.element_kids(id) {
            let c = counts.entry(k.name.1.clone()).or_insert(0);
            *c += 1;
            let kloc = format!("{}/*:{}[{}]", loc, k.name.1, c);
            match self.col.elements.get(&k.name) {
                Some(d) => self.element(k.id, d, &kloc)?,
                None => self.lax_children(k.id, &kloc)?,
            }
        }
        Ok(())
    }

    fn attributes(&self, id: usize, uses: &[AttrUse], wild: Option<&Wildcard>, loc: &str) -> Result<(), XsdError> {
        let View::Elem { attrs, .. } = self.doc.view(id) else { return Ok(()) };
        let mut seen: Vec<&Name> = vec![];
        for (k, v) in attrs {
            if k == "xmlns" || k.starts_with("xmlns:") {
                continue;
            }
            let n: Name = match k.split_once(':') {
                Some((p, l)) => (ns_in_scope(self.doc, id, p).unwrap_or_default(), l.to_string()),
                None => (String::new(), k.clone()),
            };
            if n.0 == XSI {
                continue;
            }
            match uses.iter().find(|u| u.name == n) {
                Some(u) => {
                    if u.prohibited {
                        return Err(verr(6914, format!("XML Validation: The attribute '{}' is prohibited. Location: {}.", show_name(&n), loc)));
                    }
                    let val = self.col.simple_value(&u.ty, v).map_err(|m| verr(6926, format!("XML Validation: Invalid simple type value: {}. Location: {}.", m, loc)))?;
                    if let Some(f) = &u.fixed {
                        let fv = self.col.simple_value(&u.ty, f).unwrap_or_default();
                        if fv != val {
                            return Err(verr(6926, format!("XML Validation: The value '{}' of attribute '{}' is not the fixed value '{}'. Location: {}.", val, show_name(&n), fv, loc)));
                        }
                    }
                    seen.push(&u.name);
                }
                None => match wild {
                    Some(w) if w.any || w.namespaces.iter().any(|x| *x == n.0) || w.other_than.as_ref().is_some_and(|t| *t != n.0 && !n.0.is_empty()) => {
                        if w.process == Process::Strict {
                            match self.col.attributes.get(&n) {
                                Some(u) => {
                                    self.col.simple_value(&u.ty, v).map_err(|m| verr(6926, format!("XML Validation: Invalid simple type value: {}. Location: {}.", m, loc)))?;
                                }
                                None => return Err(verr(6914, format!("XML Validation: Declaration not found for attribute '{}'. Location: {}.", show_name(&n), loc))),
                            }
                        }
                    }
                    _ => return Err(verr(6914, format!("XML Validation: Declaration not found for attribute '{}'. Location: {}.", show_name(&n), loc))),
                },
            }
        }
        for u in uses {
            if u.required && !seen.contains(&&u.name) {
                return Err(verr(6909, format!("XML Validation: Required attribute '{}' is missing. Location: {}.", show_name(&u.name), loc)));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(schema: &str, xml: &str) -> Result<(), XsdError> {
        let c = Collection::compile(&[schema.to_string()]).unwrap();
        c.validate(&Doc::parse(xml).unwrap(), true)
    }

    const ORDER: &str = r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
      <xs:element name="order">
        <xs:complexType>
          <xs:sequence>
            <xs:element name="id" type="xs:int"/>
            <xs:element name="item" maxOccurs="unbounded">
              <xs:complexType>
                <xs:simpleContent>
                  <xs:extension base="xs:string"><xs:attribute name="qty" type="xs:positiveInteger" use="required"/></xs:extension>
                </xs:simpleContent>
              </xs:complexType>
            </xs:element>
            <xs:element name="note" type="xs:string" minOccurs="0"/>
          </xs:sequence>
          <xs:attribute name="status" use="optional">
            <xs:simpleType><xs:restriction base="xs:string"><xs:enumeration value="new"/><xs:enumeration value="paid"/></xs:restriction></xs:simpleType>
          </xs:attribute>
        </xs:complexType>
      </xs:element>
    </xs:schema>"#;

    #[test]
    fn a_valid_document_passes_and_errors_are_found() {
        assert!(check(ORDER, r#"<order status="new"><id>7</id><item qty="2">pen</item><item qty="1">ink</item></order>"#).is_ok());
        assert!(check(ORDER, r#"<order><id>7</id><item qty="2">pen</item><note>x</note></order>"#).is_ok());
        assert_eq!(check(ORDER, r#"<order><id>x</id><item qty="2"/></order>"#).unwrap_err().number, 6926);
        assert_eq!(check(ORDER, r#"<order><id>1</id></order>"#).unwrap_err().number, 6965, "an item is required");
        assert_eq!(check(ORDER, r#"<order><id>1</id><item>pen</item></order>"#).unwrap_err().number, 6909, "qty is required");
        assert_eq!(check(ORDER, r#"<order><id>1</id><item qty="0">pen</item></order>"#).unwrap_err().number, 6926, "qty is positive");
        assert_eq!(check(ORDER, r#"<order status="old"><id>1</id><item qty="1"/></order>"#).unwrap_err().number, 6926);
        assert_eq!(check(ORDER, r#"<order><item qty="1"/><id>1</id></order>"#).unwrap_err().number, 6965, "order of a sequence");
        assert_eq!(check(ORDER, r#"<order bogus="1"><id>1</id><item qty="1"/></order>"#).unwrap_err().number, 6914);
        assert_eq!(check(ORDER, r#"<other/>"#).unwrap_err().number, 6913);
        assert_eq!(check(ORDER, r#"<order><id>1</id><item qty="1"/><x/></order>"#).unwrap_err().number, 6965);
        let e = check(ORDER, r#"<order><id>1</id><item qty="1"/><x/></order>"#).unwrap_err();
        assert!(e.message.contains("'note'") && e.message.contains("where element 'x'"), "{}", e.message);
    }

    #[test]
    fn simple_types_and_facets() {
        let s = r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
          <xs:simpleType name="Code"><xs:restriction base="xs:string"><xs:pattern value="[A-Z]{2}\d{3}"/><xs:maxLength value="5"/></xs:restriction></xs:simpleType>
          <xs:simpleType name="Pct"><xs:restriction base="xs:decimal"><xs:minInclusive value="0"/><xs:maxInclusive value="100"/><xs:fractionDigits value="1"/></xs:restriction></xs:simpleType>
          <xs:simpleType name="Ints"><xs:list itemType="xs:int"/></xs:simpleType>
          <xs:simpleType name="IntOrWord"><xs:union memberTypes="xs:int xs:NCName"/></xs:simpleType>
          <xs:element name="r"><xs:complexType><xs:sequence>
            <xs:element name="code" type="Code" minOccurs="0"/><xs:element name="pct" type="Pct" minOccurs="0"/>
            <xs:element name="ints" type="Ints" minOccurs="0"/><xs:element name="u" type="IntOrWord" minOccurs="0"/>
            <xs:element name="d" type="xs:date" minOccurs="0"/><xs:element name="b" type="xs:byte" minOccurs="0"/>
          </xs:sequence></xs:complexType></xs:element></xs:schema>"#;
        for ok in ["<r><code>AB123</code></r>", "<r><pct>99.5</pct></r>", "<r><ints> 1  2 3 </ints></r>", "<r><u>12</u></r>", "<r><u>abc</u></r>", "<r><d>2024-02-29</d></r>", "<r><b>-128</b></r>", "<r/>"] {
            assert!(check(s, ok).is_ok(), "{}", ok);
        }
        for bad in ["<r><code>ab123</code></r>", "<r><code>AB1234</code></r>", "<r><pct>100.5</pct></r>", "<r><pct>5.55</pct></r>", "<r><ints>1 x</ints></r>", "<r><u>a b</u></r>", "<r><d>2023-02-29</d></r>", "<r><d>2024-13-01</d></r>", "<r><b>128</b></r>"] {
            assert_eq!(check(s, bad).unwrap_err().number, 6926, "{}", bad);
        }
    }

    #[test]
    fn choice_all_groups_extension_and_wildcards() {
        let s = r###"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" targetNamespace="urn:t" xmlns="urn:t" elementFormDefault="qualified">
          <xs:group name="g"><xs:sequence><xs:element name="x" type="xs:string"/><xs:element name="y" type="xs:string"/></xs:sequence></xs:group>
          <xs:complexType name="Base"><xs:sequence><xs:element name="a" type="xs:string"/></xs:sequence><xs:attribute name="id" type="xs:ID"/></xs:complexType>
          <xs:complexType name="Derived"><xs:complexContent><xs:extension base="Base"><xs:sequence><xs:element name="b" type="xs:string"/></xs:sequence></xs:extension></xs:complexContent></xs:complexType>
          <xs:element name="d" type="Derived"/>
          <xs:element name="c"><xs:complexType><xs:choice maxOccurs="3"><xs:element name="p" type="xs:int"/><xs:element name="q" type="xs:int"/></xs:choice></xs:complexType></xs:element>
          <xs:element name="al"><xs:complexType><xs:all><xs:element name="m" type="xs:int"/><xs:element name="n" type="xs:int" minOccurs="0"/></xs:all></xs:complexType></xs:element>
          <xs:element name="gr"><xs:complexType><xs:group ref="g"/></xs:complexType></xs:element>
          <xs:element name="w"><xs:complexType mixed="true"><xs:sequence><xs:any namespace="##other" processContents="skip" minOccurs="0" maxOccurs="unbounded"/></xs:sequence><xs:anyAttribute processContents="skip"/></xs:complexType></xs:element>
          <xs:element name="nl" type="xs:int" nillable="true"/>
        </xs:schema>"###;
        let ns = r#" xmlns="urn:t""#;
        assert!(check(s, &format!("<d{ns} id=\"i1\"><a>1</a><b>2</b></d>")).is_ok());
        assert_eq!(check(s, &format!("<d{ns}><a>1</a></d>")).unwrap_err().number, 6965, "the extension adds b");
        assert!(check(s, &format!("<c{ns}><p>1</p><q>2</q><p>3</p></c>")).is_ok());
        assert_eq!(check(s, &format!("<c{ns}><p>1</p><q>2</q><p>3</p><q>4</q></c>")).unwrap_err().number, 6965, "at most three");
        assert!(check(s, &format!("<al{ns}><n>1</n><m>2</m></al>")).is_ok());
        assert!(check(s, &format!("<al{ns}><m>2</m></al>")).is_ok());
        assert_eq!(check(s, &format!("<al{ns}><n>1</n></al>")).unwrap_err().number, 6965);
        assert!(check(s, &format!("<gr{ns}><x>1</x><y>2</y></gr>")).is_ok());
        assert!(check(s, &format!("<w{ns} k=\"1\">text<o:z xmlns:o=\"urn:o\"><anything/></o:z>more</w>")).is_ok());
        assert_eq!(check(s, &format!("<w{ns}><z/></w>")).unwrap_err().number, 6965, "##other excludes the target namespace");
        assert!(check(s, &format!("<nl{ns} xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:nil=\"true\"/>")).is_ok());
        assert_eq!(check(s, "<d><a>1</a><b>2</b></d>").unwrap_err().number, 6913, "no namespace: no declaration");
    }

    #[test]
    fn a_schema_that_is_wrong_does_not_compile() {
        let bad = |s: &str| Collection::compile(&[s.to_string()]).is_err();
        assert!(bad("<a/>"));
        assert!(bad("<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"><xs:element name=\"a\" type=\"nothere\"/></xs:schema>"));
        assert!(bad("<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"><xs:element name=\"a\" type=\"xs:nothere\"/></xs:schema>"));
        assert!(bad("<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"><xs:element name=\"a\"/><xs:element name=\"a\"/></xs:schema>"));
        assert!(bad("<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"><xs:element name=\"a\"><xs:complexType><xs:sequence><xs:element ref=\"zzz\"/></xs:sequence></xs:complexType></xs:element></xs:schema>"));
        assert!(bad("not xml"));
    }
}
