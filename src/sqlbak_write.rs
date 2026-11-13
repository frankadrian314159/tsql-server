//! Writing the database image of a SQL Server backup: the inverse of [`crate::sqlbak`].
//!
//! A [`Image`] describes a database (the tables with their columns, indexes and constraints, the modules, the schemas, the
//! full-text catalogs) in the types [`crate::sqlbak`] reads; [`write_pages`] turns it, and the rows that a [`RowSource`]
//! delivers, into 8 KiB pages laid out the way the engine of SQL Server lays them out: the system base tables
//! (`sysschobjs`, `syscolpars`, `sysrscols`, ...) as records on data pages, the rows of each user table as records whose
//! layout `sysrscols` describes, large values on text pages, a boot page, a file header and the allocation maps.
//! [`crate::mtfwrite`] puts the pages in a backup file.
//!
//! **What is not written**: the pages of nonclustered indexes and the B-tree levels above the leaves of a clustered one (this
//! server rebuilds indexes from the rows), the log, and statistics. A real SQL Server would find the allocation structures
//! incomplete: the file is meant to be read back by this server (`RESTORE`) and by tools that read the pages.
use crate::sqlbak::{fixed_size, ColMeta, FtCatalogMeta, IndexMeta, ModuleMeta, TableMeta, PAGE};
use crate::types::{DtKind, Value};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

type Res<T> = Result<T, String>;

/// Everything about a database that the backup records, except the rows.
pub struct Image {
    pub name: String,
    /// (schema id, name)
    pub schemas: Vec<(u32, String)>,
    pub tables: Vec<TableMeta>,
    pub modules: Vec<ModuleMeta>,
    pub ft_catalogs: Vec<FtCatalogMeta>,
}

/// Delivers the rows of a table, one at a time. A row holds the table's stored columns (those that are not computed) in
/// column id order, the order in which [`crate::sqlbak::Backup::rows`] returns them; for a table with a clustered index the
/// rows come in key order.
pub trait RowSource {
    fn rows(&mut self, table: &TableMeta, f: &mut dyn FnMut(&[Value]) -> Res<()>) -> Res<()>;
}

#[derive(Debug, Default)]
pub struct Stats {
    pub pages: u32,
    pub tables: usize,
    pub rows: u64,
    /// values of a form that is not written (they are stored as NULL), as (table.column, count)
    pub lost: Vec<(String, u64)>,
}

// ---------------------------------------------------------------------------------------------------- the page file
const FIRST_PAGE: u32 = 10;
/// Pages that one PFS page covers; the first is page 1, then every 8088th.
const PFS_SPAN: u32 = 8088;
/// Pages that one GAM interval covers: 64,000 extents.
const GAM_SPAN: u32 = 511_232;

/// The pages of the data file, written as they are produced.
pub struct Pager {
    f: File,
    next: u32,
    /// PFS byte of each page
    pfs: Vec<u8>,
}

impl Pager {
    pub fn create(path: &Path) -> std::io::Result<Pager> {
        Ok(Pager { f: File::options().read(true).write(true).create(true).truncate(true).open(path)?, next: FIRST_PAGE, pfs: vec![0; FIRST_PAGE as usize] })
    }

    fn alloc(&mut self) -> Res<u32> {
        let mut id = self.next;
        // the PFS pages sit at fixed places
        while id % PFS_SPAN == 0 {
            id += 1;
        }
        if id >= GAM_SPAN {
            return Err("the database is larger than this writer handles (one allocation interval of about 4 GB)".into());
        }
        self.next = id + 1;
        if self.pfs.len() <= id as usize {
            self.pfs.resize(id as usize + 1, 0);
        }
        self.pfs[id as usize] = 0x44;
        Ok(id)
    }

    fn put(&mut self, id: u32, page: &[u8]) -> Res<()> {
        debug_assert_eq!(page.len(), PAGE);
        self.f.seek(SeekFrom::Start(id as u64 * PAGE as u64)).and_then(|_| self.f.write_all(page)).map_err(|e| e.to_string())
    }

    /// The file as a reader over its pages, and the number of pages.
    pub fn into_file(self) -> (File, u64) {
        (self.f, self.next as u64)
    }
}

/// A page being filled with records.
struct PageBuf {
    id: u32,
    typ: u8,
    level: u8,
    obj: u32,
    idx: u16,
    prev: u32,
    pminlen: u16,
    buf: Vec<u8>,
    free: usize,
    slots: Vec<u16>,
}

impl PageBuf {
    fn new(id: u32, typ: u8, obj: u32, idx: u16, prev: u32, pminlen: u16) -> PageBuf {
        PageBuf { id, typ, level: 0, obj, idx, prev, pminlen, buf: vec![0u8; PAGE], free: 96, slots: vec![] }
    }

    fn fits(&self, n: usize) -> bool {
        // (the free offset can pass the slot array: a page that is full has nothing left)
        PAGE.saturating_sub(2 * (self.slots.len() + 1) + self.free) >= n
    }

    fn add(&mut self, rec: &[u8]) -> u16 {
        self.buf[self.free..self.free + rec.len()].copy_from_slice(rec);
        self.slots.push(self.free as u16);
        self.free += rec.len();
        // the records of a page start on even offsets
        if self.free % 2 == 1 {
            self.free += 1;
        }
        (self.slots.len() - 1) as u16
    }

    fn finish(mut self, next: u32) -> Vec<u8> {
        let b = &mut self.buf;
        b[0] = 1;
        b[1] = self.typ;
        b[3] = self.level;
        b[6..8].copy_from_slice(&self.idx.to_le_bytes());
        b[8..12].copy_from_slice(&self.prev.to_le_bytes());
        if self.prev != 0 {
            b[12..14].copy_from_slice(&1u16.to_le_bytes());
        }
        b[14..16].copy_from_slice(&self.pminlen.to_le_bytes());
        b[16..20].copy_from_slice(&next.to_le_bytes());
        if next != 0 {
            b[20..22].copy_from_slice(&1u16.to_le_bytes());
        }
        b[22..24].copy_from_slice(&(self.slots.len() as u16).to_le_bytes());
        b[24..28].copy_from_slice(&self.obj.to_le_bytes());
        let free_cnt = PAGE.saturating_sub(2 * self.slots.len() + self.free);
        b[28..30].copy_from_slice(&(free_cnt as u16).to_le_bytes());
        b[30..32].copy_from_slice(&(self.free as u16).to_le_bytes());
        b[32..36].copy_from_slice(&self.id.to_le_bytes());
        b[36..38].copy_from_slice(&1u16.to_le_bytes());
        for (i, off) in self.slots.iter().enumerate() {
            b[PAGE - 2 * (i + 1)..PAGE - 2 * i].copy_from_slice(&off.to_le_bytes());
        }
        self.buf
    }
}

/// The pages of one object (a table, an index, a large-object store): records are appended, pages linked in order.
struct Sink {
    obj: u32,
    idx: u16,
    typ: u8,
    pminlen: u16,
    cur: Option<PageBuf>,
    first: u32,
    pages: u32,
}

impl Sink {
    fn new(obj: u32, idx: u16, typ: u8, pminlen: u16) -> Sink {
        Sink { obj, idx, typ, pminlen, cur: None, first: 0, pages: 0 }
    }

    /// Append a record; the page and slot it went to.
    fn add(&mut self, p: &mut Pager, rec: &[u8]) -> Res<(u32, u16)> {
        if rec.len() > PAGE - 96 - 2 {
            return Err(format!("a record of {} bytes does not fit on a page", rec.len()));
        }
        if self.cur.as_ref().is_some_and(|c| !c.fits(rec.len())) {
            let old = self.cur.take().unwrap();
            let id = p.alloc()?;
            let prev = old.id;
            let page = old.finish(id);
            p.put(prev, &page)?;
            self.pages += 1;
            self.cur = Some(PageBuf::new(id, self.typ, self.obj, self.idx, prev, self.pminlen));
        }
        if self.cur.is_none() {
            let id = p.alloc()?;
            if self.first == 0 {
                self.first = id;
            }
            self.cur = Some(PageBuf::new(id, self.typ, self.obj, self.idx, 0, self.pminlen));
        }
        let c = self.cur.as_mut().unwrap();
        let id = c.id;
        let slot = c.add(rec);
        Ok((id, slot))
    }

    /// Write the last page; the first page and the number of pages (0, 0 for an object without records).
    fn finish(mut self, p: &mut Pager) -> Res<(u32, u32)> {
        if let Some(c) = self.cur.take() {
            let id = c.id;
            p.put(id, &c.finish(0))?;
            self.pages += 1;
        }
        Ok((self.first, self.pages))
    }
}

// ----------------------------------------------------------------------------------------------------- the records
/// A little-endian writer of the fixed part of a record.
#[derive(Default)]
struct Fx(Vec<u8>);

impl Fx {
    fn u8(mut self, v: u8) -> Fx {
        self.0.push(v);
        self
    }
    fn u16(mut self, v: u16) -> Fx {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(mut self, v: u32) -> Fx {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(mut self, v: u64) -> Fx {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(mut self, v: &[u8]) -> Fx {
        self.0.extend_from_slice(v);
        self
    }
    fn zeros(mut self, n: usize) -> Fx {
        self.0.extend(std::iter::repeat_n(0u8, n));
        self
    }
}

/// A record: status bytes, the fixed part, the column count, the null bitmap, the variable columns (data, and whether the
/// column holds a pointer to data stored elsewhere).
fn record(fixed: &[u8], ncols: usize, nulls: &[usize], vars: &[(Vec<u8>, bool)]) -> Vec<u8> {
    let mut r = vec![0x10 | if vars.is_empty() { 0 } else { 0x20 }, 0];
    r.extend_from_slice(&((4 + fixed.len()) as u16).to_le_bytes());
    r.extend_from_slice(fixed);
    r.extend_from_slice(&(ncols as u16).to_le_bytes());
    let mut bits = vec![0u8; ncols.div_ceil(8)];
    for &n in nulls {
        bits[n / 8] |= 1 << (n % 8);
    }
    r.extend_from_slice(&bits);
    if !vars.is_empty() {
        r.extend_from_slice(&(vars.len() as u16).to_le_bytes());
        let mut end = r.len() + 2 * vars.len();
        for (d, complex) in vars {
            end += d.len();
            r.extend_from_slice(&((end as u16) | if *complex { 0x8000 } else { 0 }).to_le_bytes());
        }
        for (d, _) in vars {
            r.extend_from_slice(d);
        }
    }
    // a record is at least 9 bytes long
    while r.len() < 9 {
        r.push(0);
    }
    r
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

fn cp1252(s: &str) -> Vec<u8> {
    crate::tds::wire::to_cp1252(s)
}

/// A `datetime` in the 8 bytes of a row.
fn datetime_bytes(dt: NaiveDateTime) -> Res<[u8; 8]> {
    let base = NaiveDate::from_ymd_opt(1900, 1, 1).unwrap();
    let days = (dt.date() - base).num_days();
    let ms = dt.time().num_seconds_from_midnight() as i64 * 1000 + (dt.time().nanosecond() / 1_000_000) as i64;
    let ticks = (ms * 3 + 5) / 10;
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&(ticks as u32).to_le_bytes());
    b[4..].copy_from_slice(&(days as i32).to_le_bytes());
    Ok(b)
}

fn date_bytes(d: NaiveDate) -> [u8; 3] {
    use chrono::Datelike;
    let n = (d.num_days_from_ce() - 1) as u32;
    [n as u8, (n >> 8) as u8, (n >> 16) as u8]
}

fn time_bytes(t: NaiveTime, scale: u8) -> Vec<u8> {
    let scale = scale.min(7);
    let n = match scale {
        0..=2 => 3,
        3..=4 => 4,
        _ => 5,
    };
    let nanos = t.num_seconds_from_midnight() as u64 * 1_000_000_000 + t.nanosecond() as u64;
    let units = nanos / 10u64.pow(9 - scale as u32);
    units.to_le_bytes()[..n].to_vec()
}

fn as_i64(v: &Value) -> Res<i64> {
    match v {
        Value::Int(i) => Ok(*i as i64),
        Value::BigInt(i) => Ok(*i),
        Value::Bit(b) => Ok(*b as i64),
        Value::Dec(d) => Ok(scaled(d, 0) as i64),
        Value::Float(f) => Ok(*f as i64),
        other => Err(format!("{:?} is not an integer", other)),
    }
}

fn as_f64(v: &Value) -> Res<f64> {
    match v {
        Value::Float(f) => Ok(*f),
        Value::Int(i) => Ok(*i as f64),
        Value::BigInt(i) => Ok(*i as f64),
        Value::Dec(d) => Ok(d.m as f64 / 10f64.powi(d.s as i32)),
        other => Err(format!("{:?} is not a number", other)),
    }
}

/// A decimal's mantissa at `scale`.
fn scaled(d: &crate::decimal::Dec, scale: u8) -> i128 {
    match scale.cmp(&d.s) {
        std::cmp::Ordering::Equal => d.m,
        std::cmp::Ordering::Greater => d.m * 10i128.pow((scale - d.s) as u32),
        std::cmp::Ordering::Less => {
            let q = 10i128.pow((d.s - scale) as u32);
            let (n, r) = (d.m / q, d.m % q);
            if r.abs() * 2 >= q {
                n + d.m.signum()
            } else {
                n
            }
        }
    }
}

fn as_dec(v: &Value, scale: u8) -> Res<i128> {
    match v {
        Value::Dec(d) => Ok(scaled(d, scale)),
        Value::Int(i) => Ok(*i as i128 * 10i128.pow(scale as u32)),
        Value::BigInt(i) => Ok(*i as i128 * 10i128.pow(scale as u32)),
        Value::Float(f) => Ok((f * 10f64.powi(scale as i32)).round() as i128),
        other => Err(format!("{:?} is not a number", other)),
    }
}

/// The bytes of a fixed-length column (`fixed_size(c)` of them).
fn fixed_bytes(c: &ColMeta, v: &Value) -> Res<Vec<u8>> {
    let size = fixed_size(c);
    let b = match c.xtype {
        48 => vec![as_i64(v)? as u8],
        52 => (as_i64(v)? as i16).to_le_bytes().to_vec(),
        56 => (as_i64(v)? as i32).to_le_bytes().to_vec(),
        127 => as_i64(v)?.to_le_bytes().to_vec(),
        59 => (as_f64(v)? as f32).to_le_bytes().to_vec(),
        62 => as_f64(v)?.to_le_bytes().to_vec(),
        60 => (as_dec(v, 4)? as i64).to_le_bytes().to_vec(),
        122 => (as_dec(v, 4)? as i32).to_le_bytes().to_vec(),
        36 => match v {
            Value::Guid(g) => g.to_bytes_le().to_vec(),
            other => return Err(format!("{:?} is not a uniqueidentifier", other)),
        },
        61 => match v {
            Value::DateTime(dt, _) => datetime_bytes(*dt)?.to_vec(),
            other => return Err(format!("{:?} is not a datetime", other)),
        },
        58 => match v {
            Value::DateTime(dt, _) => {
                let base = NaiveDate::from_ymd_opt(1900, 1, 1).unwrap();
                let days = (dt.date() - base).num_days() as u16;
                let mins = (dt.time().num_seconds_from_midnight() / 60) as u16;
                [mins.to_le_bytes(), days.to_le_bytes()].concat()
            }
            other => return Err(format!("{:?} is not a smalldatetime", other)),
        },
        40 => match v {
            Value::Date(d) => date_bytes(*d).to_vec(),
            Value::DateTime(dt, _) => date_bytes(dt.date()).to_vec(),
            other => return Err(format!("{:?} is not a date", other)),
        },
        41 => match v {
            Value::Time(t) => time_bytes(*t, c.scale),
            other => return Err(format!("{:?} is not a time", other)),
        },
        42 => match v {
            Value::DateTime(dt, _) => [time_bytes(dt.time(), c.scale), date_bytes(dt.date()).to_vec()].concat(),
            other => return Err(format!("{:?} is not a datetime2", other)),
        },
        43 => match v {
            Value::Dto(d) => {
                let utc = d.naive_utc();
                let off = (d.offset().local_minus_utc() / 60) as i16;
                [time_bytes(utc.time(), c.scale), date_bytes(utc.date()).to_vec(), off.to_le_bytes().to_vec()].concat()
            }
            other => return Err(format!("{:?} is not a datetimeoffset", other)),
        },
        106 | 108 => {
            let m = as_dec(v, c.scale)?;
            let mut b = vec![if m < 0 { 0 } else { 1 }];
            b.extend_from_slice(&m.unsigned_abs().to_le_bytes()[..size - 1]);
            b
        }
        239 => match v {
            Value::Str(s) => {
                let mut u: Vec<u16> = s.encode_utf16().collect();
                u.resize(size / 2, 0x20);
                u.iter().flat_map(|x| x.to_le_bytes()).collect()
            }
            other => return Err(format!("{:?} is not text", other)),
        },
        175 => match v {
            Value::Str(s) => {
                let mut b = cp1252(s);
                b.resize(size, b' ');
                b
            }
            other => return Err(format!("{:?} is not text", other)),
        },
        173 | 189 => match v {
            Value::Bin(x) => {
                let mut b = x.clone();
                b.resize(size, 0);
                b
            }
            other => return Err(format!("{:?} is not binary", other)),
        },
        other => return Err(format!("the type {} is not a fixed-length type", other)),
    };
    debug_assert_eq!(b.len(), size, "type {}", c.xtype);
    Ok(b)
}

/// A `sql_variant`: the base type, a byte that is 1, the properties of the type and the value. None when the form of the
/// base type is not one that is known.
fn variant_bytes(v: &Value) -> Option<Vec<u8>> {
    let mut out = vec![];
    match v {
        Value::Int(i) => {
            out.extend_from_slice(&[56, 1]);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Value::BigInt(i) => {
            out.extend_from_slice(&[127, 1]);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Value::Bit(b) => out.extend_from_slice(&[104, 1, *b as u8]),
        Value::Float(f) => {
            out.extend_from_slice(&[62, 1]);
            out.extend_from_slice(&f.to_le_bytes());
        }
        Value::Guid(g) => {
            out.extend_from_slice(&[36, 1]);
            out.extend_from_slice(&g.to_bytes_le());
        }
        Value::Date(d) => {
            out.extend_from_slice(&[40, 1]);
            out.extend_from_slice(&date_bytes(*d));
        }
        Value::DateTime(dt, DtKind::DateTime) => {
            out.extend_from_slice(&[61, 1]);
            out.extend_from_slice(&datetime_bytes(*dt).ok()?);
        }
        Value::Str(s) => {
            // nvarchar: the maximum length in bytes, the collation id, the text
            out.extend_from_slice(&[231, 1]);
            out.extend_from_slice(&8000u16.to_le_bytes());
            out.extend_from_slice(&COLLATION_ID.to_le_bytes());
            out.extend_from_slice(&utf16(s));
        }
        Value::Bin(b) => {
            out.extend_from_slice(&[165, 1]);
            out.extend_from_slice(&8000u16.to_le_bytes());
            out.extend_from_slice(b);
        }
        _ => return None,
    }
    Some(out)
}

/// `SQL_Latin1_General_CP1_CI_AS`.
const COLLATION_ID: u32 = 872_468_488;

/// The bytes of a variable-length column; None when the value is of a form that is not written.
fn var_bytes(c: &ColMeta, v: &Value) -> Res<Option<Vec<u8>>> {
    let text = |v: &Value| -> Res<String> {
        match v {
            Value::Str(s) => Ok(s.clone()),
            other => Err(format!("{:?} is not text", other)),
        }
    };
    Ok(Some(match c.xtype {
        231 | 99 => utf16(&text(v)?),
        167 | 35 => cp1252(&text(v)?),
        165 | 34 => match v {
            Value::Bin(b) => b.clone(),
            other => return Err(format!("{:?} is not binary", other)),
        },
        240 => match v {
            Value::Bin(b) if c.utype == 129 || c.utype == 130 => crate::spatial::external(b),
            Value::Bin(b) => b.clone(),
            other => return Err(format!("{:?} is not a CLR value", other)),
        },
        241 => crate::binxml::from_xml(&text(v)?)?,
        98 => match variant_bytes(v) {
            Some(b) => b,
            None => return Ok(None),
        },
        other => return Err(format!("the type {} is not a variable-length type", other)),
    }))
}

pub fn is_fixed(xtype: u8) -> bool {
    matches!(xtype, 48 | 52 | 56 | 127 | 104 | 106 | 108 | 60 | 122 | 62 | 59 | 175 | 239 | 173 | 36 | 40 | 41 | 42 | 43 | 61 | 58 | 189)
}

// ------------------------------------------------------------------------------------------------ record layouts
#[derive(Clone, Copy, Debug)]
enum Place {
    /// position in the fixed part, size, and for a bit column its bit
    Fixed(usize, usize, Option<u8>),
    Var(usize),
}

/// How the rows of a table are laid out: for each stored column (in the order of its row) where it is and its null bit.
struct Layout {
    /// (index into the table's columns, place, null bit)
    cols: Vec<(usize, Place, usize)>,
    fixed_len: usize,
    stored: usize,
    /// the key column positions of the clustered index, by column id (for `sysrscols`)
    key_pos: std::collections::HashMap<u32, u16>,
}

/// Columns are numbered the way the B-tree stores them: the clustered key columns first, in key order, then the others by
/// column id; computed columns are left out. Fixed-length columns follow each other in the fixed part (bit columns share
/// bytes); variable-length ones are counted in that order.
fn layout(t: &TableMeta) -> Layout {
    let key: Vec<u32> = t.indexes.iter().find(|i| i.clustered).map(|i| i.keys.iter().map(|k| k.0).collect()).unwrap_or_default();
    let mut order: Vec<usize> = vec![];
    for k in &key {
        if let Some(i) = t.cols.iter().position(|c| c.colid == *k && !c.computed) {
            order.push(i);
        }
    }
    let mut rest: Vec<usize> = (0..t.cols.len()).filter(|i| !t.cols[*i].computed && !order.contains(i)).collect();
    rest.sort_by_key(|i| t.cols[*i].colid);
    order.extend(rest);
    let (mut pos, mut nvar) = (0usize, 0usize);
    let mut bit_byte: Option<(usize, u8)> = None;
    let mut cols = vec![];
    for (rank, &ci) in order.iter().enumerate() {
        let c = &t.cols[ci];
        let place = if c.xtype == 104 {
            let (p, bit) = match bit_byte {
                Some((p, n)) if n < 8 => (p, n),
                _ => {
                    let p = pos;
                    pos += 1;
                    (p, 0)
                }
            };
            bit_byte = Some((p, bit + 1));
            Place::Fixed(p, 1, Some(bit))
        } else if is_fixed(c.xtype) {
            let size = fixed_size(c);
            let p = pos;
            pos += size;
            Place::Fixed(p, size, None)
        } else {
            nvar += 1;
            Place::Var(nvar - 1)
        };
        cols.push((ci, place, rank));
    }
    let key_pos = key.iter().enumerate().map(|(i, k)| (*k, i as u16 + 1)).collect();
    Layout { cols, fixed_len: pos, stored: order.len(), key_pos }
}

/// The text pages of a table's large values.
struct Lob {
    sink: Sink,
    /// values that were left out, per column
    lost: std::collections::HashMap<String, u64>,
}

const FRAGMENT: usize = 8040;
/// Links that fit in a root pointer in the row, and in an internal node.
const MAX_ROOT_LINKS: usize = 600;
const NODE_LINKS: usize = 500;

impl Lob {
    fn fragment(&mut self, p: &mut Pager, data: &[u8]) -> Res<(u32, u16)> {
        let mut r = vec![0u8; 14];
        r[2..4].copy_from_slice(&((14 + data.len()) as u16).to_le_bytes());
        r[12..14].copy_from_slice(&3u16.to_le_bytes());
        r.extend_from_slice(data);
        self.sink.add(p, &r)
    }

    /// The links (cumulative size, page, slot) to the fragments of `data`, or to internal nodes over them when there are many.
    fn links(&mut self, p: &mut Pager, data: &[u8]) -> Res<Vec<(u32, u32, u16)>> {
        let mut frags = vec![];
        let mut done = 0usize;
        if data.is_empty() {
            let (pg, sl) = self.fragment(p, &[])?;
            return Ok(vec![(0, pg, sl)]);
        }
        for chunk in data.chunks(FRAGMENT) {
            let (pg, sl) = self.fragment(p, chunk)?;
            done += chunk.len();
            frags.push((done as u32, pg, sl));
        }
        if frags.len() <= MAX_ROOT_LINKS {
            return Ok(frags);
        }
        // internal nodes: 16-byte links to fragments
        let mut nodes = vec![];
        for group in frags.chunks(NODE_LINKS) {
            let mut r = vec![0u8; 20];
            r[2..4].copy_from_slice(&((20 + 16 * group.len()) as u16).to_le_bytes());
            r[12..14].copy_from_slice(&2u16.to_le_bytes());
            r[16..20].copy_from_slice(&(group.len() as u32).to_le_bytes());
            for (size, pg, sl) in group {
                let mut l = [0u8; 16];
                l[0..4].copy_from_slice(&size.to_le_bytes());
                l[8..12].copy_from_slice(&pg.to_le_bytes());
                l[12..14].copy_from_slice(&1u16.to_le_bytes());
                l[14..16].copy_from_slice(&sl.to_le_bytes());
                r.extend_from_slice(&l);
            }
            let (pg, sl) = self.sink.add(p, &r)?;
            nodes.push((group.last().unwrap().0, pg, sl));
        }
        Ok(nodes)
    }

    fn link_bytes(links: &[(u32, u32, u16)]) -> Vec<u8> {
        let mut b = vec![];
        for (size, pg, sl) in links {
            b.extend_from_slice(&size.to_le_bytes());
            b.extend_from_slice(&pg.to_le_bytes());
            b.extend_from_slice(&1u16.to_le_bytes());
            b.extend_from_slice(&sl.to_le_bytes());
        }
        b
    }

    /// The pointer that a `(max)` value or an overflowing column leaves in its row.
    fn pointer(&mut self, p: &mut Pager, data: &[u8]) -> Res<Vec<u8>> {
        let links = self.links(p, data)?;
        let mut b = vec![0u8; 12];
        b[0..2].copy_from_slice(&4u16.to_le_bytes());
        b[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&Self::link_bytes(&links));
        Ok(b)
    }

    /// The 16-byte pointer of a `text`, `ntext` or `image` column: it leads to a root record in a text page.
    fn text_pointer(&mut self, p: &mut Pager, data: &[u8], blob_id: u32) -> Res<Vec<u8>> {
        let links = self.links(p, data)?;
        let mut r = vec![0u8; 24];
        r[2..4].copy_from_slice(&((24 + 12 * links.len()) as u16).to_le_bytes());
        r[12..14].copy_from_slice(&5u16.to_le_bytes());
        r[16..20].copy_from_slice(&(links.len() as u32).to_le_bytes());
        r.extend_from_slice(&Self::link_bytes(&links));
        let (pg, sl) = self.sink.add(p, &r)?;
        let mut b = vec![0u8; 16];
        b[4..8].copy_from_slice(&blob_id.to_le_bytes());
        b[8..12].copy_from_slice(&pg.to_le_bytes());
        b[12..14].copy_from_slice(&1u16.to_le_bytes());
        b[14..16].copy_from_slice(&sl.to_le_bytes());
        Ok(b)
    }
}

/// The record of a row.
fn encode_row(t: &TableMeta, lay: &Layout, row_of: &[usize], row: &[Value], lob: &mut Lob, p: &mut Pager, blob_seq: &mut u32) -> Res<Vec<u8>> {
    let mut fixed = vec![0u8; lay.fixed_len];
    let mut nulls: Vec<usize> = vec![];
    let mut vars: Vec<(Vec<u8>, bool)> = vec![];
    for &(ci, place, nullbit) in &lay.cols {
        let c = &t.cols[ci];
        let v = row.get(row_of[ci]).unwrap_or(&Value::Null);
        let ctx = |e: String| format!("{}.{} column {}: {}", t.schema, t.name, c.name, e);
        if v.is_null() {
            nulls.push(nullbit);
            if matches!(place, Place::Var(_)) {
                vars.push((vec![], false));
            }
            continue;
        }
        match place {
            Place::Fixed(pos, _, Some(bit)) => {
                if matches!(v, Value::Bit(true)) || matches!(v, Value::Int(n) if *n != 0) {
                    fixed[pos] |= 1 << bit;
                }
            }
            Place::Fixed(pos, size, None) => {
                let b = fixed_bytes(c, v).map_err(ctx)?;
                fixed[pos..pos + size].copy_from_slice(&b);
            }
            Place::Var(_) => match var_bytes(c, v).map_err(ctx)? {
                None => {
                    // a value of a form that is not written is stored as NULL
                    *lob.lost.entry(format!("{}.{}.{}", t.schema, t.name, c.name)).or_default() += 1;
                    nulls.push(nullbit);
                    vars.push((vec![], false));
                }
                Some(b) if matches!(c.xtype, 34 | 35 | 99) => {
                    *blob_seq += 1;
                    vars.push((lob.text_pointer(p, &b, *blob_seq)?, true));
                }
                Some(b) => vars.push((b, false)),
            },
        }
    }
    let mut size = |vars: &[(Vec<u8>, bool)]| 4 + fixed.len() + 2 + lay.stored.div_ceil(8) + if vars.is_empty() { 0 } else { 2 + 2 * vars.len() + vars.iter().map(|v| v.0.len()).sum::<usize>() };
    // a row that does not fit on a page keeps its largest values off the row
    while size(&vars) > 8060 {
        let big = vars.iter().enumerate().filter(|(_, v)| !v.1 && v.0.len() > 24).max_by_key(|(_, v)| v.0.len()).map(|(i, _)| i);
        let Some(i) = big else { return Err(format!("a row of {}.{} is too large for a page", t.schema, t.name)) };
        let data = std::mem::take(&mut vars[i].0);
        vars[i] = (lob.pointer(p, &data)?, true);
    }
    Ok(record(&fixed, lay.stored, &nulls, &vars))
}

// -------------------------------------------------------------------------------------------- the system tables
const SYSALLOCUNITS: u32 = 7;
const SYSROWSETS: u32 = 5;
const SYSSCHOBJS: u32 = 34;
const SYSCOLPARS: u32 = 41;
const SYSCLSOBJS: u32 = 64;
const SYSRSCOLS: u32 = 3;
const SYSISCOLS: u32 = 55;
const SYSIDXSTATS: u32 = 54;
const SYSOBJVALUES: u32 = 60;
const SYSFTINDS: u32 = 25;
const SYSSINGLEOBJREFS: u32 = 74;

/// The records of every system table, collected before they are written.
#[derive(Default)]
struct Sys {
    allocunits: Vec<Vec<u8>>,
    rowsets: Vec<Vec<u8>>,
    rscols: Vec<Vec<u8>>,
    schobjs: Vec<Vec<u8>>,
    colpars: Vec<Vec<u8>>,
    clsobjs: Vec<Vec<u8>>,
    iscols: Vec<Vec<u8>>,
    idxstats: Vec<Vec<u8>>,
    objvalues: Vec<Vec<u8>>,
    ftinds: Vec<Vec<u8>>,
    singlerefs: Vec<Vec<u8>>,
}

fn now() -> [u8; 8] {
    datetime_bytes(chrono::Utc::now().naive_utc()).unwrap_or([0; 8])
}

fn page_ptr(page: u32) -> [u8; 6] {
    let mut b = [0u8; 6];
    if page != 0 {
        b[..4].copy_from_slice(&page.to_le_bytes());
        b[4..].copy_from_slice(&1u16.to_le_bytes());
    }
    b
}

impl Sys {
    fn schobj(&mut self, id: u32, name: &str, nsid: u32, ty: &[u8; 2], pid: u32, intprop: u32, status: u32) {
        let fixed = Fx::default().u32(id).u32(nsid).u8(1).u32(status).bytes(ty).u32(pid).u8(1).u32(intprop).bytes(&now()).bytes(&now()).u32(0).0;
        self.schobjs.push(record(&fixed, 13, &[], &[(utf16(name), false)]));
    }

    fn clsobj(&mut self, class: u8, id: u32, status: u32, ty: &[u8; 2], name: &str) {
        let fixed = Fx::default().u8(class).u32(id).u32(status).bytes(ty).u32(0).bytes(&now()).bytes(&now()).0;
        self.clsobjs.push(record(&fixed, 8, &[], &[(utf16(name), false)]));
    }

    /// A piece of text in `sysobjvalues` (8-bit characters, in pieces).
    fn text_value(&mut self, class: u8, objid: u32, subobjid: u32, text: &str) {
        let bytes = cp1252(text);
        for (n, piece) in bytes.chunks(4000).enumerate().chain(if bytes.is_empty() { Some((0, &[][..])) } else { None }) {
            let fixed = Fx::default().u8(class).u32(objid).u32(subobjid).u32(n as u32).0;
            // value (sql_variant) is NULL, imageval holds the text
            self.objvalues.push(record(&fixed, 6, &[4], &[(vec![], false), (piece.to_vec(), false)]));
        }
    }

    fn single_ref(&mut self, class: u8, depid: u32, depsubid: u32, indepid: u32, indepsubid: u32) {
        let fixed = Fx::default().u8(class).u32(depid).u32(depsubid).u32(indepid).u32(indepsubid).u32(0).0;
        self.singlerefs.push(record(&fixed, 6, &[], &[]));
    }

    fn iscol(&mut self, table: u32, index: u32, subid: u32, status: u32, colid: u32, pos: u8) {
        let fixed = Fx::default().u32(table).u32(index).u32(subid).u32(status).u32(colid).u8(pos).u8(0).u8(0).u8(0).0;
        self.iscols.push(record(&fixed, 9, &[], &[]));
    }

    fn idxstat(&mut self, table: u32, indid: u32, status: u32, ty: u8, tinyprop: u8, name: &str) {
        let fixed = Fx::default().u32(table).u32(indid).u32(status).u32(0).u8(0).u8(ty).u8(tinyprop).u32(1).u32(0).u32(0).0;
        self.idxstats.push(record(&fixed, 11, &[], &[(utf16(name), false)]));
    }
}

fn type_info(c: &ColMeta) -> (u8, i16, u8, u8) {
    // (xtype, length, precision, scale) as `sys.columns` shows them; the precision of the date and time types follows from the scale
    let ts = |base: u8| if c.scale > 0 { base + c.scale + 1 } else { base };
    let prec = match c.xtype {
        48 => 3,
        52 => 5,
        56 => 10,
        127 => 19,
        104 => 1,
        59 => 24,
        62 => 53,
        60 => 19,
        122 => 10,
        61 => 23,
        58 => 16,
        40 => 10,
        41 => ts(8),
        42 => ts(19),
        43 => ts(26),
        106 | 108 => c.prec,
        _ => 0,
    };
    (c.xtype, c.length, prec, c.scale)
}

fn column_status(c: &ColMeta) -> u32 {
    let mut s = 0;
    if !c.nullable {
        s |= 1;
    }
    if matches!(c.xtype, 167 | 175 | 231 | 239 | 165 | 173) {
        s |= 2;
    }
    if c.identity.is_some() {
        s |= 4;
    }
    if c.computed {
        s |= 0x90;
    }
    s
}

fn identity_bytes(c: &ColMeta) -> Option<Vec<u8>> {
    let (seed, inc, last) = c.identity?;
    let size = match c.xtype {
        48 => 1,
        52 => 2,
        56 => 4,
        127 => 8,
        _ => return None,
    };
    let mut b = vec![];
    for v in [last, seed, inc] {
        b.extend_from_slice(&v.to_le_bytes()[..size]);
    }
    Some(b)
}

/// Write the whole image. `src` supplies the rows. The pages go to `pager`.
pub fn write_pages(img: &Image, src: &mut dyn RowSource, p: &mut Pager) -> Res<Stats> {
    let mut stats = Stats { tables: img.tables.len(), ..Stats::default() };
    let mut sys = Sys::default();
    let mut next_obj = img.tables.iter().map(|t| t.id).chain(img.modules.iter().map(|m| m.id)).max().unwrap_or(1000).max(1000) + 1;
    let mut take_id = || {
        next_obj += 1;
        next_obj - 1
    };
    let schema_id = |name: &str| img.schemas.iter().find(|s| s.1.eq_ignore_ascii_case(name)).map(|s| s.0).unwrap_or(1);
    for (id, name) in &img.schemas {
        sys.clsobj(50, *id, 0, b"  ", name);
    }
    for c in &img.ft_catalogs {
        sys.clsobj(32, c.id, c.is_default as u32 | if c.accent_sensitive { 2 } else { 0 }, b"  ", &c.name);
    }
    let mut blob_seq = 0u32;
    for t in &img.tables {
        let lay = layout(t);
        let clustered = t.indexes.iter().any(|i| i.clustered);
        let idx: u16 = clustered as u16;
        let obj = t.id;
        sys.schobj(obj, &t.name, schema_id(&t.schema), b"U ", 0, 0, 0);
        // the rows
        let mut data = Sink::new(obj, idx, 1, (4 + lay.fixed_len) as u16);
        let mut lob = Lob { sink: Sink::new(obj, idx, 3, 0), lost: Default::default() };
        let stored: Vec<usize> = (0..t.cols.len()).filter(|i| !t.cols[*i].computed).collect();
        // row_of[column index] = position of the column in a delivered row
        let mut row_of = vec![0usize; t.cols.len()];
        for (pos, ci) in stored.iter().enumerate() {
            row_of[*ci] = pos;
        }
        let mut rows = 0u64;
        {
            let (data, lob, blob_seq, rows) = (&mut data, &mut lob, &mut blob_seq, &mut rows);
            src.rows(t, &mut |row: &[Value]| {
                let rec = encode_row(t, &lay, &row_of, row, lob, p, blob_seq)?;
                data.add(p, &rec)?;
                *rows += 1;
                Ok(())
            })?;
        }
        stats.rows += rows;
        let (first, npages) = data.finish(p)?;
        let (lob_first, lob_pages) = lob.sink.finish(p)?;
        for (k, n) in lob.lost {
            stats.lost.push((k, n));
        }
        // sysrowsets / sysallocunits
        let rowset: u64 = ((idx as u64) << 48) | ((obj as u64) << 16);
        let rs = Fx::default().u64(rowset).u8(1).u32(obj).u32(idx as u32).u32(1).u32(0).u16(1).u64(rows).u8(0).u8(0).u16(0).u32(0).u16(0).u16(0).u16(0).zeros(16).zeros(8).u32(0).0;
        sys.rowsets.push(record(&rs, 18, &[], &[]));
        for (ty, pg, n, low) in [(1u8, first, npages, 0u64), (2u8, lob_first, lob_pages, 1u64)] {
            if pg == 0 {
                continue;
            }
            let au = Fx::default().u64(rowset | low).u8(ty).u64(rowset).u32(0).u16(1).bytes(&page_ptr(pg)).bytes(&page_ptr(pg)).bytes(&page_ptr(0)).u64(n as u64).u64(n as u64).u64(n as u64).0;
            sys.allocunits.push(record(&au, 11, &[], &[]));
        }
        // sysrscols: where each stored column is
        for &(ci, place, nullbit) in &lay.cols {
            let c = &t.cols[ci];
            let (off, bit) = match place {
                Place::Fixed(pos, _, bit) => ((4 + pos) as u32, bit.unwrap_or(0) as u16),
                Place::Var(k) => (0xFFFF - k as u32, 0),
            };
            let ti = c.xtype as u32 | ((c.length as u16 as u32) << 8);
            let maxin = match place {
                Place::Fixed(_, s, _) => s as u16,
                Place::Var(_) => 0,
            };
            let fx = Fx::default()
                .u64(rowset)
                .u32(c.colid)
                .u32(nullbit as u32 + 1)
                .u64(0)
                .u32(ti)
                .u32(COLLATION_ID)
                .u16(lay.key_pos.get(&c.colid).copied().unwrap_or(0))
                .u16(maxin)
                .u32(0)
                .u32(off)
                .u32(nullbit as u32 + 1)
                .u16(bit)
                .zeros(16)
                .u32(0)
                .0;
            sys.rscols.push(record(&fx, 14, &[], &[]));
        }
        // syscolpars
        for c in &t.cols {
            let (xtype, length, prec, scale) = type_info(c);
            let fx = Fx::default()
                .u32(obj)
                .u16(c.colid as u16)
                .u32(c.colid)
                .u8(xtype)
                .u32(if xtype == 240 { c.utype } else { xtype as u32 })
                .u16(length as u16)
                .u8(prec)
                .u8(scale)
                .u32(if matches!(xtype, 167 | 175 | 231 | 239 | 35 | 99) { COLLATION_ID } else { 0 })
                .u32(column_status(c))
                .u16(0)
                .u32(0)
                .u32(0)
                .u32(0)
                .0;
            let mut vars = vec![(utf16(&c.name), false)];
            if let Some(idt) = identity_bytes(c) {
                vars.push((idt, false));
            }
            sys.colpars.push(record(&fx, 16, &[], &vars));
            if c.computed {
                if let Some(d) = &c.definition {
                    sys.text_value(2, obj, c.colid, d);
                }
            }
        }
        // indexes (the definitions only: this server rebuilds them from the rows)
        let mut write_index = |sys: &mut Sys, ix: &IndexMeta| {
            let status = if ix.unique { 8 } else { 0 } | if ix.primary { 0x20 } else { 0 } | if ix.unique_constraint { 0x40 } else { 0 };
            sys.idxstat(obj, ix.id, status, if ix.clustered { 1 } else { 2 }, 0, &ix.name);
            for (n, (col, desc)) in ix.keys.iter().enumerate() {
                sys.iscol(obj, ix.id, n as u32 + 1, if *desc { 4 } else { 0 }, *col, n as u8 + 1);
            }
            for (n, col) in ix.included.iter().enumerate() {
                sys.iscol(obj, ix.id, (ix.keys.len() + n) as u32 + 1, 0x10, *col, 0);
            }
        };
        for ix in &t.indexes {
            write_index(&mut sys, ix);
        }
        let mut xid = 256_000u32;
        for x in &t.xml_indexes {
            xid += 1;
            sys.idxstat(obj, xid, 0, 3, if x.primary { 0 } else { x.secondary.map(|c| c as u8).unwrap_or(b'V') }, &x.name);
            sys.iscol(obj, xid, 1, 0, x.column, 1);
        }
        for (name, col) in &t.spatial_indexes {
            xid += 1;
            sys.idxstat(obj, xid, 0, 4, 0, name);
            sys.iscol(obj, xid, 1, 0, *col, 1);
        }
        // constraints
        for d in &t.defaults {
            let id = take_id();
            sys.schobj(id, &d.name, schema_id(&t.schema), b"D ", obj, d.column, 0);
            sys.text_value(1, id, 0, &d.definition);
        }
        for c in &t.checks {
            let id = take_id();
            sys.schobj(id, &c.name, schema_id(&t.schema), b"C ", obj, c.column, 0);
            sys.text_value(1, id, 0, &c.definition);
        }
        for f in &t.foreign_keys {
            let id = take_id();
            sys.schobj(id, &f.name, schema_id(&t.schema), b"F ", obj, 0, if f.cascade_delete { 0x4000 } else { 0 });
            sys.single_ref(27, id, 0, f.ref_table, 0);
            for (n, (local, remote)) in f.columns.iter().enumerate() {
                sys.single_ref(28, id, n as u32 + 1, 0, *local);
                sys.single_ref(29, id, n as u32 + 1, 0, *remote);
            }
        }
        if let Some(ft) = &t.fulltext {
            let fx = Fx::default().u32(obj).u32(ft.key_index).u32(0).u32(0).0;
            sys.ftinds.push(record(&fx, 4, &[], &[]));
            for (col, lcid) in &ft.cols {
                let fx = Fx::default().u8(42).u32(obj).u32(*col).u32(*lcid as u32).0;
                sys.objvalues.push(record(&fx, 6, &[4, 5], &[(vec![], false), (vec![], false)]));
            }
            sys.single_ref(39, obj, 0, ft.catalog, 0);
        }
    }
    for m in &img.modules {
        let ty: [u8; 2] = m.kind;
        sys.schobj(m.id, &m.name, schema_id(&m.schema), &ty, m.parent, 0, 0);
        if let Some(def) = &m.definition {
            sys.text_value(1, m.id, 1, def);
        }
    }
    // the system tables themselves
    let mut write_sys = |recs: &[Vec<u8>], obj: u32, idx: u16, p: &mut Pager| -> Res<()> {
        let mut s = Sink::new(obj, idx, 1, recs.first().map(|r| u16::from_le_bytes([r[2], r[3]])).unwrap_or(0));
        for r in recs {
            s.add(p, r)?;
        }
        s.finish(p)?;
        Ok(())
    };
    let first_alloc = p.next;
    write_sys(&sys.rowsets, SYSROWSETS, 0, p)?;
    write_sys(&sys.rscols, SYSRSCOLS, 0, p)?;
    write_sys(&sys.schobjs, SYSSCHOBJS, 1, p)?;
    write_sys(&sys.colpars, SYSCOLPARS, 1, p)?;
    write_sys(&sys.clsobjs, SYSCLSOBJS, 1, p)?;
    write_sys(&sys.iscols, SYSISCOLS, 1, p)?;
    write_sys(&sys.idxstats, SYSIDXSTATS, 1, p)?;
    write_sys(&sys.objvalues, SYSOBJVALUES, 1, p)?;
    write_sys(&sys.ftinds, SYSFTINDS, 1, p)?;
    write_sys(&sys.singlerefs, SYSSINGLEOBJREFS, 1, p)?;
    // sysallocunits last: its own first page is recorded in the boot page
    let alloc_first = p.next;
    write_sys(&sys.allocunits, SYSALLOCUNITS, 0, p)?;
    let _ = first_alloc;
    boot_and_maps(img, p, alloc_first)?;
    stats.pages = p.next;
    Ok(stats)
}

/// Page 0 (the file header), the allocation maps (PFS, GAM, SGAM) and the boot page.
fn boot_and_maps(img: &Image, p: &mut Pager, alloc_first: u32) -> Res<()> {
    let total = p.next;
    let mut header = |id: u32, typ: u8| {
        let mut b = vec![0u8; PAGE];
        b[0] = 1;
        b[1] = typ;
        b[32..36].copy_from_slice(&id.to_le_bytes());
        b[36..38].copy_from_slice(&1u16.to_le_bytes());
        b[22..24].copy_from_slice(&1u16.to_le_bytes());
        b
    };
    // the file header
    let mut b = header(0, 15);
    b[96..98].copy_from_slice(&1u16.to_le_bytes());
    b[100..104].copy_from_slice(&total.to_le_bytes());
    p.put(0, &b)?;
    // PFS pages: a byte for each page of the interval (0x40: allocated)
    let mut base = 0u32;
    while base < total {
        let id = if base == 0 { 1 } else { base };
        let mut b = header(id, 11);
        for k in 0..PFS_SPAN {
            let page = base + k;
            if page >= total {
                break;
            }
            let flag = if page < FIRST_PAGE || page == base || (page % PFS_SPAN == 0 && page > 0) { 0x40 } else { p.pfs.get(page as usize).copied().unwrap_or(0) };
            b[100 + k as usize] = flag;
        }
        p.put(id, &b)?;
        base += PFS_SPAN;
    }
    // GAM (a bit per extent, 0: allocated) and SGAM: every extent of the file is in use
    for (id, typ) in [(2u32, 8u8), (3, 9)] {
        let b = header(id, typ);
        p.put(id, &b)?;
    }
    // the boot page: the database name (UTF-16, padded with spaces) and the first page of sysallocunits
    let mut b = header(9, 13);
    let mut name: Vec<u16> = img.name.encode_utf16().collect();
    name.resize(128, 0x20);
    for (i, u) in name.iter().enumerate() {
        b[0x94 + 2 * i..0x94 + 2 * i + 2].copy_from_slice(&u.to_le_bytes());
    }
    b[0x264..0x268].copy_from_slice(&alloc_first.to_le_bytes());
    b[0x268..0x26A].copy_from_slice(&1u16.to_le_bytes());
    p.put(9, &b)?;
    Ok(())
}
