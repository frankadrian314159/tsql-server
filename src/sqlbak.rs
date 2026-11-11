//! The database inside a native SQL Server backup: pages, system tables, user tables and their rows.
//!
//! The expanded backup ([`crate::mssqlbak`], [`crate::mtf`]) holds, in an `MQDA` stream per data file, the **allocated** 8 KiB
//! pages of that file in file order; each page header says which page it is. [`Backup`] indexes the pages, reads the
//! system base tables it needs (`sysschobjs`, `syscolpars`, `sysclsobjs`, `sysrowsets`, `sysallocunits`) the way the
//! engine of SQL Server lays them out, and then decodes the rows of any user table.
//!
//! What is decoded: fixed- and variable-length columns of the common types, NULLs, computed columns (which are not stored),
//! bit columns, identity properties. Values stored off the row (large objects, row overflow) and the binary form of `xml`
//! are not read yet: such a value comes back as NULL and is counted in [`Rows::skipped`].
use crate::decimal::Dec;
use crate::mssqlbak::Expanded;
use crate::mtf;
use crate::types::{DataType, DtKind, Value};
use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

pub const PAGE: usize = 8192;
const MAX_SLOTS: usize = PAGE / 2;

type Res<T> = Result<T, String>;

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|x| u16::from_le_bytes([x[0], x[1]]))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|x| u32::from_le_bytes(x.try_into().unwrap()))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o + 8).map(|x| u64::from_le_bytes(x.try_into().unwrap()))
}

/// A record in a page: the bytes of the page and where the record starts.
struct Rec<'a> {
    p: &'a [u8],
    off: usize,
    /// offset of the column count, from the start of the record (the end of the fixed part)
    fl: usize,
    ncols: usize,
    nullbits: &'a [u8],
    nvar: usize,
    var_ends: usize,
    status: u8,
}

impl<'a> Rec<'a> {
    fn new(p: &'a [u8], off: usize) -> Option<Rec<'a>> {
        let status = *p.get(off)?;
        let fl = u16_at(p, off + 2)? as usize;
        if fl < 4 {
            return None;
        }
        let ncols = u16_at(p, off + fl)? as usize;
        let nb = ncols.div_ceil(8);
        let nullbits = p.get(off + fl + 2..off + fl + 2 + nb)?;
        let q = off + fl + 2 + nb;
        let nvar = if status & 0x20 != 0 { u16_at(p, q)? as usize } else { 0 };
        Some(Rec { p, off, fl, ncols, nullbits, nvar, var_ends: q + 2, status })
    }

    /// 0 primary, 1 forwarded, 2 forwarding stub, 3 index, 4 blob fragment, 5.. ghosts
    fn kind(&self) -> u8 {
        (self.status >> 1) & 7
    }

    fn is_null(&self, i: usize) -> bool {
        i >= self.ncols || self.nullbits.get(i / 8).is_some_and(|b| b >> (i % 8) & 1 != 0)
    }

    /// `n` bytes of the fixed part at `pos` (counted from the start of the fixed columns).
    fn fixed(&self, pos: usize, n: usize) -> Option<&'a [u8]> {
        if 4 + pos + n > self.fl {
            return None;
        }
        self.p.get(self.off + 4 + pos..self.off + 4 + pos + n)
    }

    fn fu32(&self, pos: usize) -> u32 {
        self.fixed(pos, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0)
    }
    fn fu16(&self, pos: usize) -> u16 {
        self.fixed(pos, 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0)
    }
    fn fu64(&self, pos: usize) -> u64 {
        self.fixed(pos, 8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).unwrap_or(0)
    }

    /// Variable-length column `i` (counting only variable columns): its bytes and whether it is a *complex* column (the
    /// high bit of its end offset), which holds a pointer to data stored elsewhere.
    fn var(&self, i: usize) -> Option<(&'a [u8], bool)> {
        if i >= self.nvar {
            return None;
        }
        let end_of = |k: usize| u16_at(self.p, self.var_ends + 2 * k);
        let start = if i == 0 { self.var_ends + 2 * self.nvar } else { self.off + (end_of(i - 1)? & 0x7FFF) as usize };
        let e = end_of(i)?;
        let end = self.off + (e & 0x7FFF) as usize;
        if end < start {
            return None;
        }
        Some((self.p.get(start..end)?, e & 0x8000 != 0))
    }
}

fn utf16(b: &[u8]) -> String {
    let u: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&u)
}

#[derive(Debug, Clone)]
pub struct ColMeta {
    pub colid: u32,
    pub name: String,
    pub xtype: u8,
    pub utype: u32,
    pub length: i16,
    pub prec: u8,
    pub scale: u8,
    pub status: u32,
    /// the engine's type for the column
    pub ty: DataType,
    pub nullable: bool,
    /// (seed, increment, last value)
    pub identity: Option<(i64, i64, i64)>,
    /// computed columns are not stored in the row
    pub computed: bool,
}

#[derive(Debug, Clone)]
pub struct TableMeta {
    pub id: u32,
    pub schema: String,
    pub name: String,
    pub cols: Vec<ColMeta>,
    /// the object number and index number that the pages of the table's rows carry in their headers, and whether the
    /// table is clustered (rows come in key order)
    pub data: Option<(u32, u16, bool)>,
    /// the row count that the catalog states
    pub rows: u64,
    /// where each stored column is in the records, by column id (from `sysrscols`)
    pub locs: HashMap<u32, ColLoc>,
}

/// Where a column is in a record.
#[derive(Debug, Clone, Copy, Default)]
pub struct ColLoc {
    /// fixed-length columns: the position in the fixed part, counted after the 4-byte record header
    pub fixed: Option<usize>,
    /// variable-length columns: the index among the variable columns
    pub var: Option<usize>,
    /// the index of the column's bit in the null bitmap
    pub nullbit: usize,
    /// bit columns: the bit within the byte at `fixed`
    pub bitpos: Option<u8>,
}

#[derive(Debug, Default)]
pub struct Rows {
    pub rows: Vec<Vec<Value>>,
    /// values left NULL because they are stored off the row or in a form that is not decoded, per column name
    pub skipped: HashMap<String, u64>,
}

pub struct Backup<R: Read + Seek> {
    ex: Expanded<R>,
    /// (file id, page id) -> offset of the page in the expanded stream
    loc: HashMap<(u16, u32), u64>,
    /// (object, index) from the page header -> pages of type 1 (data) at level 0
    by_obj: HashMap<(u32, u16), Vec<u32>>,
    /// page -> (previous page, next page), for the pages in `by_obj`
    links: HashMap<u32, (u32, u32)>,
    pub db_name: String,
    schemas: HashMap<u32, String>,
}

fn page_ok(p: &[u8]) -> bool {
    // header version 1, a known page type, a plausible slot count; some runs of pages look like noise
    p.len() >= 96 && p[0] == 1 && matches!(p[1], 1 | 2 | 3 | 4 | 7 | 8 | 9 | 10 | 11 | 13 | 15 | 16 | 17) && u16_at(p, 22).is_some_and(|n| (n as usize) <= MAX_SLOTS)
}

// the object numbers of the system base tables whose records are read
const SYSALLOCUNITS: u32 = 7;
const SYSROWSETS: u32 = 5;
const SYSSCHOBJS: u32 = 34;
const SYSCOLPARS: u32 = 41;
const SYSCLSOBJS: u32 = 64;
const SYSRSCOLS: u32 = 3;
const SYSISCOLS: u32 = 55;

impl<R: Read + Seek> Backup<R> {
    /// Index the pages of a backup whose container has been opened.
    pub fn open(mut ex: Expanded<R>) -> Res<Backup<R>> {
        let scan = mtf::scan(&mut ex).map_err(|e| e.to_string())?;
        if let Some(p) = &scan.problem {
            return Err(format!("the backup's stream is damaged: {}", p));
        }
        let mut loc = HashMap::new();
        let mut by_obj: HashMap<(u32, u16), Vec<u32>> = HashMap::new();
        let mut links = HashMap::new();
        for b in scan.blocks.iter().filter(|b| &b.kind == b"MSDA") {
            for s in b.streams.iter().filter(|s| &s.id == b"MQDA" && s.len > 2) {
                let n = (s.len - 2) / PAGE as u64;
                let mut page = vec![0u8; PAGE];
                for k in 0..n {
                    let at = s.data_at + 2 + k * PAGE as u64;
                    ex.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
                    ex.read_exact(&mut page).map_err(|e| e.to_string())?;
                    let (Some(pid), Some(fid)) = (u32_at(&page, 32), u16_at(&page, 36)) else { continue };
                    if fid == 0 || !page_ok(&page) {
                        continue;
                    }
                    loc.insert((fid, pid), at);
                    if page[1] == 1 && page[3] == 0 && fid == 1 {
                        let obj = u32_at(&page, 24).unwrap_or(0);
                        let idx = u16_at(&page, 6).unwrap_or(0);
                        by_obj.entry((obj, idx)).or_default().push(pid);
                        links.insert(pid, (u32_at(&page, 8).unwrap_or(0), u32_at(&page, 16).unwrap_or(0)));
                    }
                }
            }
        }
        if loc.is_empty() {
            return Err("the backup holds no database pages".to_string());
        }
        for v in by_obj.values_mut() {
            v.sort_unstable();
        }
        let mut b = Backup { ex, loc, by_obj, links, db_name: String::new(), schemas: HashMap::new() };
        b.read_boot()?;
        b.read_schemas()?;
        Ok(b)
    }

    /// Number of pages indexed.
    pub fn page_count(&self) -> usize {
        self.loc.len()
    }

    fn page(&mut self, pid: u32) -> Res<Vec<u8>> {
        let at = *self.loc.get(&(1, pid)).ok_or_else(|| format!("page 1:{} is not in the backup", pid))?;
        let mut p = vec![0u8; PAGE];
        self.ex.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
        self.ex.read_exact(&mut p).map_err(|e| e.to_string())?;
        Ok(p)
    }

    fn slots(p: &[u8]) -> Vec<usize> {
        let n = u16_at(p, 22).unwrap_or(0) as usize;
        (0..n.min(MAX_SLOTS)).filter_map(|s| u16_at(p, PAGE - 2 * (s + 1))).map(|o| o as usize).collect()
    }

    /// The pages of an allocation unit; for a clustered one in key order, found by following the links between leaf pages.
    fn leaf_pages(&self, obj: u32, idx: u16, clustered: bool) -> Vec<u32> {
        let Some(pids) = self.by_obj.get(&(obj, idx)) else { return vec![] };
        if !clustered {
            return pids.clone();
        }
        let set: std::collections::HashSet<u32> = pids.iter().copied().collect();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(pids.len());
        for &start in pids {
            // a chain starts at a page whose previous page is not one of the unit's pages
            if self.links.get(&start).is_some_and(|l| set.contains(&l.0)) {
                continue;
            }
            let mut pid = start;
            while set.contains(&pid) && seen.insert(pid) {
                out.push(pid);
                pid = self.links.get(&pid).map(|l| l.1).unwrap_or(0);
            }
        }
        // pages that are in no chain (a cycle or a damaged link) still come out
        out.extend(pids.iter().copied().filter(|p| !seen.contains(p)));
        out
    }

    /// Call `f` with every primary record of the pages of a system table.
    fn each_record(&mut self, obj: u32, idx: u16, clustered: bool, f: &mut dyn FnMut(&Rec)) -> Res<()> {
        for pid in self.leaf_pages(obj, idx, clustered) {
            let p = self.page(pid)?;
            for off in Self::slots(&p) {
                if let Some(r) = Rec::new(&p, off) {
                    if matches!(r.kind(), 0 | 1) {
                        f(&r);
                    }
                }
            }
        }
        Ok(())
    }

    fn read_boot(&mut self) -> Res<()> {
        let p = self.page(9)?;
        if p[1] != 13 {
            return Err("page 9 is not the boot page".to_string());
        }
        self.db_name = utf16(p.get(0x94..0x94 + 256).ok_or("the boot page is short")?).trim_end().to_string();
        Ok(())
    }

    fn read_schemas(&mut self) -> Res<()> {
        let mut out = HashMap::new();
        // sysclsobjs: class tinyint, id int, status int, type char(2), intprop int, created, modified; then the name
        self.each_record(SYSCLSOBJS, 1, true, &mut |r| {
            if r.fixed(0, 1).is_some_and(|c| c[0] == 50) {
                if let Some((n, false)) = r.var(0) {
                    out.insert(r.fu32(1), utf16(n));
                }
            }
        })?;
        if out.is_empty() {
            return Err("the backup has no schemas (sysclsobjs could not be read)".to_string());
        }
        self.schemas = out;
        Ok(())
    }

    /// Every user table with its columns and where its rows are.
    pub fn tables(&mut self) -> Res<Vec<TableMeta>> {
        // objects: id int, nsid int, nsclass tinyint, status int, type char(2), pid, pclass, intprop, created, modified; name
        let mut tables: Vec<TableMeta> = vec![];
        self.each_record(SYSSCHOBJS, 1, true, &mut |r| {
            if r.fixed(13, 2) == Some(b"U ") {
                let name = r.var(0).map(|(n, _)| utf16(n)).unwrap_or_default();
                tables.push(TableMeta { id: r.fu32(0), schema: r.fu32(4).to_string(), name, cols: vec![], data: None, rows: 0, locs: HashMap::new() });
            }
        })?;
        for t in &mut tables {
            let nsid: u32 = t.schema.parse().unwrap_or(0);
            t.schema = self.schemas.get(&nsid).cloned().unwrap_or_else(|| "dbo".to_string());
        }
        let mut by_id: HashMap<u32, usize> = HashMap::new();
        for (i, t) in tables.iter().enumerate() {
            by_id.insert(t.id, i);
        }
        // columns: id int, number smallint, colid int, xtype tinyint, utype int, length smallint, prec tinyint, scale tinyint,
        // collationid int, status int, maxinrow smallint, xmlns int, dflt int, chk int; name, idtval
        let mut cols: Vec<(usize, ColMeta)> = vec![];
        self.each_record(SYSCOLPARS, 1, true, &mut |r| {
            let Some(&ti) = by_id.get(&r.fu32(0)) else { return };
            let xtype = r.fixed(10, 1).map(|b| b[0]).unwrap_or(0);
            let (length, prec, scale) = (r.fu16(15) as i16, r.fixed(17, 1).map(|b| b[0]).unwrap_or(0), r.fixed(18, 1).map(|b| b[0]).unwrap_or(0));
            let status = r.fu32(23);
            let identity = r.var(1).and_then(|(b, _)| identity_of(b, xtype));
            cols.push((
                ti,
                ColMeta {
                    colid: r.fu32(6),
                    name: r.var(0).map(|(n, _)| utf16(n)).unwrap_or_default(),
                    xtype,
                    utype: r.fu32(11),
                    length,
                    prec,
                    scale,
                    status,
                    ty: col_type(xtype, length, prec, scale),
                    nullable: status & 1 == 0,
                    identity,
                    computed: status & 0x80 != 0,
                },
            ));
        })?;
        for (ti, c) in cols {
            tables[ti].cols.push(c);
        }
        for t in &mut tables {
            t.cols.sort_by_key(|c| c.colid);
        }
        // where the rows are: sysrowsets (idmajor = the object) -> sysallocunits (ownerid = the rowset)
        let mut rowsets: HashMap<u64, (u32, u32, u64)> = HashMap::new(); // rowsetid -> (idmajor, idminor, rcrows)
        self.each_record(SYSROWSETS, 0, false, &mut |r| {
            rowsets.insert(r.fu64(0), (r.fu32(9), r.fu32(13), r.fu64(27)));
        })?;
        let mut units: HashMap<u64, Vec<(u8, u64)>> = HashMap::new(); // ownerid -> (type, auid)
        self.each_record(SYSALLOCUNITS, 0, false, &mut |r| {
            let ty = r.fixed(8, 1).map(|b| b[0]).unwrap_or(0);
            units.entry(r.fu64(9)).or_default().push((ty, r.fu64(0)));
        })?;
        for t in &mut tables {
            for (rs, &(major, minor, rc)) in &rowsets {
                if major != t.id || minor > 1 {
                    continue;
                }
                t.rows += rc;
                if let Some(&(_, auid)) = units.get(rs).and_then(|v| v.iter().find(|u| u.0 == 1)) {
                    t.data = Some((((auid >> 16) & 0xFFFF_FFFF) as u32, (auid >> 48) as u16, minor == 1));
                }
            }
        }
        // the physical layout: sysrscols (rowset -> column -> offset, null bit, bit position) with the columns numbered as
        // the b-tree stores them (the clustered key first, in key order, then the others by column id; computed columns
        // are not stored)
        let mut rs_cols: HashMap<u64, HashMap<u32, ColLoc>> = HashMap::new();
        self.each_record(SYSRSCOLS, 0, false, &mut |r| {
            let (off, nullbit) = (r.fu32(40) as u16, r.fu32(44) as u16);
            let loc = if off >= 0xFF00 {
                ColLoc { fixed: None, var: Some((0xFFFFu16 - off) as usize), nullbit: (nullbit as usize).saturating_sub(1), bitpos: None }
            } else {
                ColLoc { fixed: (off as usize).checked_sub(4), var: None, nullbit: (nullbit as usize).saturating_sub(1), bitpos: Some(r.fu16(48) as u8) }
            };
            rs_cols.entry(r.fu64(0)).or_default().insert(r.fu32(12), loc);
        })?;
        // sysiscols: idmajor int, idminor int, subid int, status int, intprop int (the column id), tinyprop1 (the position in
        // the key, 0 when the column is not a key column), ...
        let mut keys: HashMap<u32, Vec<(u8, u32)>> = HashMap::new(); // table -> (position in the key, colid) of the clustered index
        self.each_record(SYSISCOLS, 1, true, &mut |r| {
            let pos = r.fixed(20, 1).map(|b| b[0]).unwrap_or(0);
            if r.fu32(4) == 1 && pos > 0 {
                keys.entry(r.fu32(0)).or_default().push((pos, r.fu32(16)));
            }
        })?;
        for t in &mut tables {
            let Some((_, _, clustered)) = t.data else { continue };
            // the rowset that holds the data
            let rs = rowsets.iter().find(|(rs, v)| v.0 == t.id && v.1 == if clustered { 1 } else { 0 } && units.get(*rs).is_some_and(|u| u.iter().any(|x| x.0 == 1))).map(|(rs, _)| *rs);
            let Some(layout) = rs.and_then(|rs| rs_cols.get(&rs)) else { continue };
            let stored: Vec<u32> = t.cols.iter().filter(|c| !c.computed).map(|c| c.colid).collect();
            let mut order: Vec<u32> = vec![];
            if clustered {
                let mut k = keys.get(&t.id).cloned().unwrap_or_default();
                k.sort_unstable();
                order.extend(k.into_iter().map(|(_, c)| c).filter(|c| stored.contains(c)));
            }
            let first = order.clone();
            order.extend(stored.iter().copied().filter(|c| !first.contains(c)));
            for (i, colid) in order.iter().enumerate() {
                if let Some(loc) = layout.get(&(i as u32 + 1)) {
                    t.locs.insert(*colid, *loc);
                }
            }
        }
        tables.sort_by(|a, b| (a.schema.as_str(), a.name.as_str()).cmp(&(b.schema.as_str(), b.name.as_str())));
        Ok(tables)
    }

    /// Decode the rows of a table (only the stored columns: computed columns are not in the pages).
    pub fn rows(&mut self, t: &TableMeta) -> Res<Rows> {
        let mut out = Rows::default();
        let Some((obj, idx, clustered)) = t.data else { return Ok(out) };
        let cols: Vec<&ColMeta> = t.cols.iter().filter(|c| !c.computed).collect();
        let plan = Plan::new(t, &cols);
        for pid in self.leaf_pages(obj, idx, clustered) {
            let p = self.page(pid)?;
            for off in Self::slots(&p) {
                let Some(r) = Rec::new(&p, off) else { continue };
                if !matches!(r.kind(), 0 | 1) {
                    continue;
                }
                out.rows.push(plan.decode(&r, &cols, &mut out.skipped));
            }
        }
        Ok(out)
    }
}

/// How the columns of a table are laid out in its records.
struct Plan {
    /// per stored column: where it is, and which bit of the null bitmap says it is NULL
    cols: Vec<(Kind, usize)>,
}

#[derive(Clone, Copy)]
enum Kind {
    /// (position in the fixed part, size)
    Fixed(usize, usize),
    /// (position in the fixed part, bit in that byte)
    Bit(usize, u8),
    /// index among the variable-length columns
    Var(usize),
    /// not found in the catalog
    Unknown,
}

fn fixed_size(c: &ColMeta) -> usize {
    match c.xtype {
        48 => 1,
        52 => 2,
        56 | 59 | 58 | 122 => 4,
        127 | 62 | 61 | 60 | 189 => 8,
        36 => 16,
        175 | 239 | 173 => c.length.max(0) as usize,
        40 => 3,
        41 | 42 | 43 => {
            let t = if c.scale <= 2 {
                3
            } else if c.scale <= 4 {
                4
            } else {
                5
            };
            t + match c.xtype {
                41 => 0,
                42 => 3,
                _ => 5,
            }
        }
        106 | 108 => match c.prec {
            0..=9 => 5,
            10..=19 => 9,
            20..=28 => 13,
            _ => 17,
        },
        _ => 0,
    }
}

impl Plan {
    fn new(t: &TableMeta, cols: &[&ColMeta]) -> Plan {
        let cols = cols
            .iter()
            .map(|c| match t.locs.get(&c.colid) {
                Some(l) => {
                    let kind = match (l.fixed, l.var, l.bitpos) {
                        (Some(pos), _, Some(bit)) if c.xtype == 104 => Kind::Bit(pos, bit),
                        (Some(pos), _, _) => Kind::Fixed(pos, fixed_size(c)),
                        (_, Some(k), _) => Kind::Var(k),
                        _ => Kind::Unknown,
                    };
                    (kind, l.nullbit)
                }
                None => (Kind::Unknown, 0),
            })
            .collect();
        Plan { cols }
    }

    fn decode(&self, r: &Rec, cols: &[&ColMeta], skipped: &mut HashMap<String, u64>) -> Vec<Value> {
        let mut row = Vec::with_capacity(cols.len());
        for (i, c) in cols.iter().enumerate() {
            let (kind, nullbit) = self.cols[i];
            if !matches!(kind, Kind::Unknown) && r.is_null(nullbit) {
                row.push(Value::Null);
                continue;
            }
            let v = match kind {
                Kind::Fixed(pos, n) => r.fixed(pos, n).and_then(|b| fixed_value(c, b)),
                Kind::Bit(pos, bit) => r.fixed(pos, 1).map(|b| Value::Bit(b[0] >> bit & 1 != 0)),
                Kind::Var(k) => match r.var(k) {
                    Some((b, false)) => var_value(c, b),
                    _ => None,
                },
                Kind::Unknown => None,
            };
            match v {
                Some(v) => row.push(v),
                None => {
                    *skipped.entry(c.name.clone()).or_default() += 1;
                    row.push(Value::Null);
                }
            }
        }
        row
    }
}

fn identity_of(b: &[u8], xtype: u8) -> Option<(i64, i64, i64)> {
    let size = match xtype {
        48 => 1,
        52 => 2,
        56 => 4,
        127 => 8,
        _ => return None,
    };
    if b.len() < 3 * size {
        return None;
    }
    let get = |k: usize| -> i64 {
        let mut v = [0u8; 8];
        v[..size].copy_from_slice(&b[k * size..(k + 1) * size]);
        let mut x = i64::from_le_bytes(v);
        if size < 8 && xtype != 48 && (x >> (size * 8 - 1)) & 1 == 1 {
            x -= 1i64 << (size * 8);
        }
        x
    };
    // stored as: last value used, seed, increment
    Some((get(1), get(2), get(0)))
}

/// The engine's type for a column of the backup.
pub fn col_type(xtype: u8, length: i16, prec: u8, scale: u8) -> DataType {
    let max = length < 0;
    match xtype {
        48 => DataType::TinyInt,
        52 => DataType::SmallInt,
        56 => DataType::Int,
        127 => DataType::BigInt,
        104 => DataType::Bit,
        106 | 108 => DataType::Decimal(prec.max(1), scale),
        60 => DataType::Money,
        122 => DataType::SmallMoney,
        62 => DataType::Float,
        59 => DataType::Real,
        175 => DataType::Char(length as i32),
        167 => DataType::VarChar(if max { crate::types::MAX } else { length as i32 }),
        239 => DataType::NChar(length as i32 / 2),
        231 => DataType::NVarChar(if max { crate::types::MAX } else { length as i32 / 2 }),
        173 => DataType::Binary(length as i32),
        165 => DataType::VarBinary(if max { crate::types::MAX } else { length as i32 }),
        36 => DataType::Guid,
        61 => DataType::DateTime,
        58 => DataType::SmallDateTime,
        40 => DataType::Date,
        41 => DataType::Time(scale),
        42 => DataType::DateTime2(scale),
        43 => DataType::DateTimeOffset(scale),
        189 => DataType::Timestamp,
        241 => DataType::Xml,
        35 => DataType::Text,
        99 => DataType::NText,
        34 => DataType::Image,
        98 => DataType::Variant,
        // CLR types (hierarchyid, geography, geometry) are kept in their binary form
        _ => DataType::VarBinary(crate::types::MAX),
    }
}

const CP1252: [char; 32] = [
    '\u{20AC}', '\u{81}', '\u{201A}', '\u{192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}', '\u{2C6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8D}', '\u{17D}', '\u{8F}', '\u{90}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}',
    '\u{2014}', '\u{2DC}', '\u{2122}', '\u{161}', '\u{203A}', '\u{153}', '\u{9D}', '\u{17E}', '\u{178}',
];

fn cp1252(b: &[u8]) -> String {
    b.iter().map(|&c| if (0x80..0xA0).contains(&c) { CP1252[(c - 0x80) as usize] } else { c as char }).collect()
}

fn time_of(b: &[u8], scale: u8) -> Option<NaiveTime> {
    let mut v = [0u8; 8];
    v[..b.len()].copy_from_slice(b);
    let units = u64::from_le_bytes(v);
    let nanos = units.checked_mul(10u64.pow(9 - scale.min(7) as u32))?;
    NaiveTime::from_num_seconds_from_midnight_opt((nanos / 1_000_000_000) as u32, (nanos % 1_000_000_000) as u32)
}

fn date_of(b: &[u8]) -> Option<NaiveDate> {
    let days = u32::from_le_bytes([b[0], b[1], b[2], 0]) as i32;
    NaiveDate::from_num_days_from_ce_opt(days + 1)
}

fn fixed_value(c: &ColMeta, b: &[u8]) -> Option<Value> {
    Some(match c.xtype {
        48 => Value::Int(b[0] as i32),
        52 => Value::Int(i16::from_le_bytes([b[0], b[1]]) as i32),
        56 => Value::Int(i32::from_le_bytes(b.try_into().ok()?)),
        127 => Value::BigInt(i64::from_le_bytes(b.try_into().ok()?)),
        59 => Value::Float(f32::from_le_bytes(b.try_into().ok()?) as f64),
        62 => Value::Float(f64::from_le_bytes(b.try_into().ok()?)),
        60 => Value::Dec(Dec::new(i64::from_le_bytes(b.try_into().ok()?) as i128, 4)),
        122 => Value::Dec(Dec::new(i32::from_le_bytes(b.try_into().ok()?) as i128, 4)),
        36 => Value::Guid(uuid::Uuid::from_bytes_le(b.try_into().ok()?)),
        61 => {
            let ticks = u32::from_le_bytes(b[..4].try_into().ok()?) as i64;
            let days = i32::from_le_bytes(b[4..].try_into().ok()?) as i64;
            let ms = (ticks * 10 + 1) / 3;
            let d = NaiveDate::from_ymd_opt(1900, 1, 1)?.and_hms_opt(0, 0, 0)?;
            Value::DateTime(d.checked_add_signed(Duration::days(days))?.checked_add_signed(Duration::milliseconds(ms))?, DtKind::DateTime)
        }
        58 => {
            let mins = u16::from_le_bytes([b[0], b[1]]) as i64;
            let days = u16::from_le_bytes([b[2], b[3]]) as i64;
            let d = NaiveDate::from_ymd_opt(1900, 1, 1)?.and_hms_opt(0, 0, 0)?;
            Value::DateTime(d.checked_add_signed(Duration::days(days))?.checked_add_signed(Duration::minutes(mins))?, DtKind::SmallDateTime)
        }
        239 => Value::Str(utf16(b)),
        175 => Value::Str(cp1252(b)),
        173 | 189 => Value::Bin(b.to_vec()),
        40 => Value::Date(date_of(b)?),
        41 => Value::Time(time_of(b, c.scale)?),
        42 => {
            let n = b.len() - 3;
            Value::DateTime(NaiveDateTime::new(date_of(&b[n..])?, time_of(&b[..n], c.scale)?), DtKind::DateTime2)
        }
        43 => {
            use chrono::TimeZone;
            let n = b.len() - 5;
            let utc = NaiveDateTime::new(date_of(&b[n..n + 3])?, time_of(&b[..n], c.scale)?);
            let off = i16::from_le_bytes([b[n + 3], b[n + 4]]) as i32;
            Value::Dto(chrono::FixedOffset::east_opt(off * 60)?.from_utc_datetime(&utc))
        }
        106 | 108 => {
            let mut m = [0u8; 16];
            m[..b.len() - 1].copy_from_slice(&b[1..]);
            let v = u128::from_le_bytes(m) as i128;
            Value::Dec(Dec::new(if b[0] == 1 { v } else { -v }, c.scale))
        }
        _ => return None,
    })
}

fn var_value(c: &ColMeta, b: &[u8]) -> Option<Value> {
    match c.xtype {
        231 => Some(Value::Str(utf16(b))),
        167 => Some(Value::Str(cp1252(b))),
        165 | 240 => Some(Value::Bin(b.to_vec())),
        241 => crate::binxml::to_xml(b).ok().map(Value::Str),
        // text, ntext, image, sql_variant: not decoded
        _ => None,
    }
}
