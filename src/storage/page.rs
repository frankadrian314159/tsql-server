//! Paged storage: a buffer pool over a page file, and the B+tree that lives in it.
//!
//! # Pages and shadow paging
//!
//! Every page has a *logical* id (a [`PageId`]) that other pages refer to, and a *physical* block in
//! the page file. The mapping (the *page table*) is kept in memory and is part of every checkpoint.
//! A block that belongs to the last checkpoint is never overwritten: when such a page is written
//! again it goes to a fresh block and the old block is released only after the next checkpoint has
//! been made durable. The page file therefore always contains a complete, consistent database as of
//! the last checkpoint, plus garbage written since then that recovery ignores. That is what lets the
//! buffer pool write *any* page back at *any* time (even changes of uncommitted transactions)
//! without a write-ahead rule or an undo pass: recovery starts from the checkpoint and replays the
//! redo log.
//!
//! # Buffer pool
//!
//! Decoded pages are cached in frames (clock replacement). When the pool is over capacity the least
//! recently used unpinned frame is written back (if dirty) and dropped. A store without a file (an
//! in-memory server) never evicts.
//!
//! # B+tree
//!
//! One tree type serves both table heaps (key = row id) and indexes (key = index key columns plus
//! row id). Entries are `(KeyVals, row id) -> Slot`; heap entries use an empty `KeyVals`. The root page
//! never moves (a root split copies the root's content into two new children), so a tree is
//! identified by its root id for its whole life. Leaves are doubly linked. Deletions do not merge
//! pages. A version chain that serialises to more than [`SPILL_AT`] bytes is stored in overflow pages.
use super::{KeyVals, Slot};
use crate::error::SqlError;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtOrd};
use std::sync::{Arc, Mutex};

pub const PAGE_SIZE: usize = 8192;
const HDR: usize = 8;
const BODY: usize = PAGE_SIZE - HDR;
/// Payload bytes of one overflow page.
const OVF_CHUNK: usize = BODY - 64;
pub type PageId = u32;
pub const NO_PAGE: PageId = u32::MAX;
const NO_BLOCK: u64 = u64::MAX;
/// A version chain that serialises to more than this many bytes lives in overflow pages.
pub const SPILL_AT: usize = 1024;
/// A node is split once its estimated size exceeds this.
const NODE_MAX: usize = 6000;
/// Longest index key (serialised) accepted; SQL Server's limit for non-clustered keys is also 1700.
pub const MAX_KEY_BYTES: usize = 1700;
pub const DEFAULT_POOL_PAGES: usize = 16384; // 128 MiB of pages

type R<T> = Result<T, String>;

pub type TKey = (KeyVals, u64);

fn cmp_tk(k1: &KeyVals, r1: u64, k2: &KeyVals, r2: u64) -> Ordering {
    k1.cmp(k2).then(r1.cmp(&r2))
}

// ------------------------------------------------------------------- nodes
#[derive(Clone, Debug)]
pub struct LeafEntry {
    pub key: KeyVals,
    pub rid: u64,
    pub slot: Slot,
    klen: u32,
    slen: u32,
}

impl LeafEntry {
    pub fn new(key: KeyVals, rid: u64, slot: Slot) -> LeafEntry {
        let klen = postcard::to_allocvec(&key).map(|v| v.len()).unwrap_or(0) as u32;
        let slen = postcard::to_allocvec(&slot).map(|v| v.len()).unwrap_or(0) as u32;
        LeafEntry { key, rid, slot, klen, slen }
    }
    fn est(&self) -> usize {
        let s = self.slen as usize;
        self.klen as usize + 12 + if s > SPILL_AT { 24 } else { s }
    }
}

#[derive(Debug)]
enum Node {
    Leaf { entries: Vec<LeafEntry>, prev: PageId, next: PageId },
    Inner { keys: Vec<TKey>, children: Vec<PageId>, leafy: bool },
}

impl Node {
    fn est(&self) -> usize {
        match self {
            Node::Leaf { entries, .. } => entries.iter().map(|e| e.est()).sum::<usize>() + 16,
            Node::Inner { keys, children, .. } => keys.iter().map(|(k, _)| postcard::to_allocvec(k).map(|v| v.len()).unwrap_or(8) + 14).sum::<usize>() + children.len() * 5 + 16,
        }
    }
}

#[derive(Serialize, Deserialize)]
enum Payload {
    Inline(Slot),
    Spilled { first: PageId, len: u32 },
}
#[derive(Serialize, Deserialize)]
enum Image {
    Leaf { prev: PageId, next: PageId, entries: Vec<(KeyVals, u64, Payload)> },
    Inner { leafy: bool, keys: Vec<TKey>, children: Vec<PageId> },
    Ovf { next: PageId, data: Vec<u8> },
}
// Borrowed mirror of `Image` used for encoding; the variant and field order must match exactly.
#[derive(Serialize)]
enum PayloadRef<'a> {
    Inline(&'a Slot),
    Spilled { first: PageId, len: u32 },
}
#[derive(Serialize)]
enum ImageRef<'a> {
    Leaf { prev: PageId, next: PageId, entries: Vec<(&'a KeyVals, u64, PayloadRef<'a>)> },
    Inner { leafy: bool, keys: &'a Vec<TKey>, children: &'a Vec<PageId> },
    Ovf { next: PageId, data: &'a [u8] },
}

struct Frame {
    node: Node,
    dirty: bool,
    refbit: bool,
    /// Overflow pages that back this leaf's on-disk image.
    chains: Vec<PageId>,
}

#[derive(Clone, Copy)]
struct PEntry {
    phys: u64,
    protected: bool,
}

/// The page table at a checkpoint.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoreImage {
    /// Physical block of each logical page (`u64::MAX` = free).
    pub table: Vec<u64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StoreStats {
    pub pool_capacity: usize,
    pub pages_cached: usize,
    pub pages_live: usize,
    pub file_blocks: u64,
    pub free_blocks: usize,
    pub reads: u64,
    pub writes: u64,
    pub hits: u64,
    pub evictions: u64,
}

struct Pool {
    file: Option<File>,
    durable: bool,
    capacity: usize,
    table: Vec<PEntry>,
    free_ids: Vec<PageId>,
    nblocks: u64,
    free_blocks: Vec<u64>,
    pending_free: Vec<u64>,
    frames: HashMap<PageId, Frame>,
    order: VecDeque<PageId>,
    pinned: Vec<PageId>,
    stats: StoreStats,
    failed: Option<String>,
}

impl Pool {
    fn evicting(&self) -> bool {
        self.file.is_some()
    }

    // ---- ids and blocks
    fn alloc_id(&mut self) -> PageId {
        if let Some(i) = self.free_ids.pop() {
            self.table[i as usize] = PEntry { phys: NO_BLOCK, protected: false };
            return i;
        }
        self.table.push(PEntry { phys: NO_BLOCK, protected: false });
        (self.table.len() - 1) as PageId
    }

    fn free_id(&mut self, id: PageId) {
        self.frames.remove(&id);
        let e = self.table[id as usize];
        if e.phys != NO_BLOCK {
            if e.protected {
                self.pending_free.push(e.phys);
            } else {
                self.free_blocks.push(e.phys);
            }
        }
        self.table[id as usize] = PEntry { phys: NO_BLOCK, protected: false };
        self.free_ids.push(id);
    }

    fn alloc_block(&mut self) -> u64 {
        if let Some(b) = self.free_blocks.pop() {
            return b;
        }
        self.nblocks += 1;
        self.nblocks - 1
    }

    fn write_bytes(&mut self, id: PageId, bytes: &[u8]) -> R<()> {
        if bytes.len() > BODY {
            return Err(format!("page {} image of {} bytes exceeds the page size", id, bytes.len()));
        }
        let e = self.table[id as usize];
        let phys = if e.phys == NO_BLOCK {
            self.alloc_block()
        } else if e.protected {
            self.pending_free.push(e.phys);
            self.alloc_block()
        } else {
            e.phys
        };
        self.table[id as usize] = PEntry { phys, protected: false };
        let f = self.file.as_mut().ok_or("no page file")?;
        let mut buf = vec![0u8; PAGE_SIZE];
        buf[0..4].copy_from_slice(&crc32fast::hash(bytes).to_le_bytes());
        buf[4..8].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        buf[HDR..HDR + bytes.len()].copy_from_slice(bytes);
        f.seek(SeekFrom::Start(phys * PAGE_SIZE as u64)).map_err(|e| format!("page write failed: {}", e))?;
        f.write_all(&buf).map_err(|e| format!("page write failed: {}", e))?;
        self.stats.writes += 1;
        Ok(())
    }

    fn read_bytes(&mut self, id: PageId) -> R<Vec<u8>> {
        let e = *self.table.get(id as usize).ok_or_else(|| format!("page {} does not exist", id))?;
        if e.phys == NO_BLOCK {
            return Err(format!("page {} has no block", id));
        }
        let f = self.file.as_mut().ok_or("no page file")?;
        let mut buf = vec![0u8; PAGE_SIZE];
        f.seek(SeekFrom::Start(e.phys * PAGE_SIZE as u64)).map_err(|e| format!("page read failed: {}", e))?;
        f.read_exact(&mut buf).map_err(|e| format!("page {} read failed: {}", id, e))?;
        let crc = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        if len > BODY || crc32fast::hash(&buf[HDR..HDR + len]) != crc {
            return Err(format!("page {} (block {}) failed its checksum: the database is corrupt", id, e.phys));
        }
        self.stats.reads += 1;
        Ok(buf[HDR..HDR + len].to_vec())
    }

    // ---- frames
    fn decode(&mut self, id: PageId) -> R<(Node, Vec<PageId>)> {
        let bytes = self.read_bytes(id)?;
        let img: Image = postcard::from_bytes(&bytes).map_err(|e| format!("page {} is corrupt: {}", id, e))?;
        let mut chains = vec![];
        let node = match img {
            Image::Leaf { prev, next, entries } => {
                let mut out = Vec::with_capacity(entries.len());
                for (k, rid, p) in entries {
                    let slot = match p {
                        Payload::Inline(s) => s,
                        Payload::Spilled { first, len } => {
                            let mut data = Vec::with_capacity(len as usize);
                            let mut cur = first;
                            while cur != NO_PAGE && data.len() < len as usize {
                                let b = self.read_bytes(cur)?;
                                match postcard::from_bytes::<Image>(&b).map_err(|e| format!("overflow page {} is corrupt: {}", cur, e))? {
                                    Image::Ovf { next, data: d } => {
                                        data.extend_from_slice(&d);
                                        chains.push(cur);
                                        cur = next;
                                    }
                                    _ => return Err(format!("page {} is not an overflow page", cur)),
                                }
                            }
                            postcard::from_bytes::<Slot>(&data).map_err(|e| format!("overflow data is corrupt: {}", e))?
                        }
                    };
                    out.push(LeafEntry::new(k, rid, slot));
                }
                Node::Leaf { entries: out, prev, next }
            }
            Image::Inner { leafy, keys, children } => Node::Inner { keys, children, leafy },
            Image::Ovf { .. } => return Err(format!("page {} is an overflow page, not a tree node", id)),
        };
        Ok((node, chains))
    }

    fn touch_new(&mut self, id: PageId) {
        if self.evicting() {
            self.order.push_back(id);
        }
    }

    fn frame(&mut self, id: PageId) -> R<&mut Frame> {
        if !self.frames.contains_key(&id) {
            let (node, chains) = self.decode(id)?;
            self.frames.insert(id, Frame { node, dirty: false, refbit: true, chains });
            self.touch_new(id);
            self.pinned.push(id);
            if self.frames.len() > self.capacity {
                self.evict_one()?;
            }
        } else {
            self.stats.hits += 1;
            if !self.pinned.contains(&id) {
                self.pinned.push(id);
            }
        }
        let f = self.frames.get_mut(&id).unwrap();
        f.refbit = true;
        Ok(f)
    }

    fn new_frame(&mut self, node: Node) -> PageId {
        let id = self.alloc_id();
        self.frames.insert(id, Frame { node, dirty: true, refbit: true, chains: vec![] });
        self.touch_new(id);
        self.pinned.push(id);
        id
    }

    fn evict_one(&mut self) -> R<bool> {
        if !self.evicting() {
            return Ok(false);
        }
        if self.order.len() > 4 * self.frames.len() + 64 {
            let frames = &self.frames;
            let mut seen = std::collections::HashSet::new();
            self.order.retain(|i| frames.contains_key(i) && seen.insert(*i));
        }
        let mut budget = self.order.len() * 2 + 2;
        while budget > 0 {
            budget -= 1;
            let id = match self.order.pop_front() {
                Some(i) => i,
                None => return Ok(false),
            };
            let (present, refbit) = match self.frames.get_mut(&id) {
                Some(f) => {
                    let r = f.refbit;
                    f.refbit = false;
                    (true, r)
                }
                None => (false, false),
            };
            if !present {
                continue;
            }
            if self.pinned.contains(&id) || refbit {
                self.order.push_back(id);
                continue;
            }
            self.flush_frame(id)?;
            self.frames.remove(&id);
            self.stats.evictions += 1;
            return Ok(true);
        }
        Ok(false)
    }

    fn trim(&mut self) -> R<()> {
        while self.evicting() && self.frames.len() > self.capacity {
            if !self.evict_one()? {
                break;
            }
        }
        Ok(())
    }

    fn flush_frame(&mut self, id: PageId) -> R<()> {
        let dirty = match self.frames.get(&id) {
            Some(f) => f.dirty,
            None => return Ok(()),
        };
        if !dirty {
            return Ok(());
        }
        let mut f = self.frames.remove(&id).unwrap();
        let res = self.encode_and_write(id, &mut f);
        if res.is_ok() {
            f.dirty = false;
        }
        self.frames.insert(id, f);
        res
    }

    fn encode_and_write(&mut self, id: PageId, f: &mut Frame) -> R<()> {
        for p in std::mem::take(&mut f.chains) {
            self.free_id(p);
        }
        let bytes = match &f.node {
            Node::Leaf { entries, prev, next } => {
                let mut ents: Vec<(&KeyVals, u64, PayloadRef)> = Vec::with_capacity(entries.len());
                let mut chains = vec![];
                for e in entries {
                    if e.slen as usize > SPILL_AT {
                        let data = postcard::to_allocvec(&e.slot).map_err(|e| e.to_string())?;
                        let first = self.write_chain(&data, &mut chains)?;
                        ents.push((&e.key, e.rid, PayloadRef::Spilled { first, len: data.len() as u32 }));
                    } else {
                        ents.push((&e.key, e.rid, PayloadRef::Inline(&e.slot)));
                    }
                }
                f.chains = chains;
                postcard::to_allocvec(&ImageRef::Leaf { prev: *prev, next: *next, entries: ents }).map_err(|e| e.to_string())?
            }
            Node::Inner { keys, children, leafy } => postcard::to_allocvec(&ImageRef::Inner { leafy: *leafy, keys, children }).map_err(|e| e.to_string())?,
        };
        self.write_bytes(id, &bytes)
    }

    fn write_chain(&mut self, data: &[u8], pages: &mut Vec<PageId>) -> R<PageId> {
        let chunks: Vec<&[u8]> = data.chunks(OVF_CHUNK).collect();
        let ids: Vec<PageId> = (0..chunks.len()).map(|_| self.alloc_id()).collect();
        for (i, c) in chunks.iter().enumerate() {
            let next = ids.get(i + 1).copied().unwrap_or(NO_PAGE);
            let bytes = postcard::to_allocvec(&ImageRef::Ovf { next, data: c }).map_err(|e| e.to_string())?;
            self.write_bytes(ids[i], &bytes)?;
        }
        pages.extend(ids.iter().copied());
        Ok(ids[0])
    }

    // ---- tree operations
    fn new_tree(&mut self) -> PageId {
        self.new_frame(Node::Leaf { entries: vec![], prev: NO_PAGE, next: NO_PAGE })
    }

    fn find_leaf(&mut self, root: PageId, first_true: &dyn Fn(&KeyVals, u64) -> bool, path: &mut Vec<(PageId, usize)>) -> R<PageId> {
        let mut id = root;
        loop {
            let f = self.frame(id)?;
            match &f.node {
                Node::Leaf { .. } => return Ok(id),
                Node::Inner { keys, children, .. } => {
                    let c = keys.partition_point(|(k, r)| !first_true(k, *r));
                    let next = children[c];
                    path.push((id, c));
                    id = next;
                }
            }
        }
    }

    fn bt_peek(&mut self, root: PageId, key: &KeyVals, rid: u64, f: &mut dyn FnMut(Option<&Slot>)) -> R<()> {
        let leaf = self.find_leaf(root, &|k, r| cmp_tk(k, r, key, rid) == Ordering::Greater, &mut vec![])?;
        if let Node::Leaf { entries, .. } = &self.frame(leaf)?.node {
            let pos = entries.partition_point(|e| cmp_tk(&e.key, e.rid, key, rid) == Ordering::Less);
            match entries.get(pos) {
                Some(e) if cmp_tk(&e.key, e.rid, key, rid) == Ordering::Equal => f(Some(&e.slot)),
                _ => f(None),
            }
        }
        Ok(())
    }

    /// Insert or replace. Returns true when the entry is new.
    fn bt_put(&mut self, root: PageId, key: KeyVals, rid: u64, slot: Slot) -> R<bool> {
        let mut path = vec![];
        let leaf = self.find_leaf(root, &|k, r| cmp_tk(k, r, &key, rid) == Ordering::Greater, &mut path)?;
        let entry = LeafEntry::new(key, rid, slot);
        let size;
        let existed;
        {
            let f = self.frame(leaf)?;
            f.dirty = true;
            let entries = match &mut f.node {
                Node::Leaf { entries, .. } => entries,
                _ => return Err("tree corrupt: expected a leaf".into()),
            };
            let pos = entries.partition_point(|e| cmp_tk(&e.key, e.rid, &entry.key, entry.rid) == Ordering::Less);
            existed = pos < entries.len() && cmp_tk(&entries[pos].key, entries[pos].rid, &entry.key, entry.rid) == Ordering::Equal;
            if existed {
                entries[pos] = entry;
            } else {
                entries.insert(pos, entry);
            }
            size = entries.iter().map(|e| e.est()).sum::<usize>() + 16;
        }
        if size > NODE_MAX {
            self.split_leaf(root, leaf, path)?;
        }
        Ok(!existed)
    }

    fn split_leaf(&mut self, root: PageId, leaf: PageId, path: Vec<(PageId, usize)>) -> R<()> {
        let (right, left_for_root, old_next) = {
            let f = self.frame(leaf)?;
            f.dirty = true;
            let (entries, next) = match &mut f.node {
                Node::Leaf { entries, next, .. } => (entries, *next),
                _ => return Err("tree corrupt: expected a leaf".into()),
            };
            if entries.len() < 2 {
                return Ok(());
            }
            let total: usize = entries.iter().map(|e| e.est()).sum();
            let mut acc = 0;
            let mut at = entries.len() / 2;
            for (i, e) in entries.iter().enumerate() {
                acc += e.est();
                if acc * 2 >= total {
                    at = (i + 1).clamp(1, entries.len() - 1);
                    break;
                }
            }
            let right = entries.split_off(at);
            let left_for_root = if leaf == root { Some(std::mem::take(entries)) } else { None };
            (right, left_for_root, next)
        };
        let sep: TKey = (right[0].key.clone(), right[0].rid);
        if let Some(left_entries) = left_for_root {
            let l = self.new_frame(Node::Leaf { entries: left_entries, prev: NO_PAGE, next: NO_PAGE });
            let r = self.new_frame(Node::Leaf { entries: right, prev: l, next: old_next });
            if let Node::Leaf { next, .. } = &mut self.frame(l)?.node {
                *next = r;
            }
            let f = self.frame(root)?;
            f.node = Node::Inner { keys: vec![sep], children: vec![l, r], leafy: true };
            f.dirty = true;
            return Ok(());
        }
        let r = self.new_frame(Node::Leaf { entries: right, prev: leaf, next: old_next });
        if let Node::Leaf { next, .. } = &mut self.frame(leaf)?.node {
            *next = r;
        }
        if old_next != NO_PAGE {
            let f = self.frame(old_next)?;
            f.dirty = true;
            if let Node::Leaf { prev, .. } = &mut f.node {
                *prev = r;
            }
        }
        self.insert_separator(root, path, sep, r)
    }

    fn insert_separator(&mut self, root: PageId, mut path: Vec<(PageId, usize)>, sep: TKey, right: PageId) -> R<()> {
        let (pid, c) = path.pop().ok_or("tree corrupt: no parent")?;
        let (too_big, leafy) = {
            let f = self.frame(pid)?;
            f.dirty = true;
            let leafy = match &mut f.node {
                Node::Inner { keys, children, leafy } => {
                    keys.insert(c, sep);
                    children.insert(c + 1, right);
                    *leafy
                }
                _ => return Err("tree corrupt: expected an inner node".into()),
            };
            (f.node.est() > NODE_MAX && keys_len(&f.node) >= 3, leafy)
        };
        if !too_big {
            return Ok(());
        }
        let (up, rkeys, rchildren, lkeys, lchildren) = {
            let f = self.frame(pid)?;
            match &mut f.node {
                Node::Inner { keys, children, .. } => {
                    let mid = keys.len() / 2;
                    let mut rkeys = keys.split_off(mid + 1);
                    let up = keys.pop().unwrap();
                    let rchildren = children.split_off(mid + 1);
                    let _ = &mut rkeys;
                    if pid == root {
                        (up, rkeys, rchildren, std::mem::take(keys), std::mem::take(children))
                    } else {
                        (up, rkeys, rchildren, vec![], vec![])
                    }
                }
                _ => unreachable!(),
            }
        };
        let r = self.new_frame(Node::Inner { keys: rkeys, children: rchildren, leafy });
        if pid == root {
            let l = self.new_frame(Node::Inner { keys: lkeys, children: lchildren, leafy });
            let f = self.frame(root)?;
            f.node = Node::Inner { keys: vec![up], children: vec![l, r], leafy: false };
            f.dirty = true;
            Ok(())
        } else {
            self.insert_separator(root, path, up, r)
        }
    }

    fn bt_remove(&mut self, root: PageId, key: &KeyVals, rid: u64) -> R<bool> {
        let leaf = self.find_leaf(root, &|k, r| cmp_tk(k, r, key, rid) == Ordering::Greater, &mut vec![])?;
        let f = self.frame(leaf)?;
        if let Node::Leaf { entries, .. } = &mut f.node {
            let pos = entries.partition_point(|e| cmp_tk(&e.key, e.rid, key, rid) == Ordering::Less);
            if pos < entries.len() && cmp_tk(&entries[pos].key, entries[pos].rid, key, rid) == Ordering::Equal {
                entries.remove(pos);
                f.dirty = true;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Visit entries starting at the first one for which `first_true` holds (forward), or walking
    /// backwards from just before it. `visit` returns false to stop.
    fn bt_scan(&mut self, root: PageId, first_true: &dyn Fn(&KeyVals, u64) -> bool, forward: bool, visit: &mut dyn FnMut(&KeyVals, u64, &Slot) -> bool) -> R<()> {
        let mut id = self.find_leaf(root, first_true, &mut vec![])?;
        let mut pos = match &self.frame(id)?.node {
            Node::Leaf { entries, .. } => entries.partition_point(|e| !first_true(&e.key, e.rid)),
            _ => 0,
        };
        loop {
            let link;
            {
                let f = self.frame(id)?;
                let (entries, prev, next) = match &f.node {
                    Node::Leaf { entries, prev, next } => (entries, *prev, *next),
                    _ => return Err("tree corrupt: expected a leaf".into()),
                };
                if forward {
                    for e in &entries[pos.min(entries.len())..] {
                        if !visit(&e.key, e.rid, &e.slot) {
                            return Ok(());
                        }
                    }
                    link = next;
                } else {
                    for e in entries[..pos.min(entries.len())].iter().rev() {
                        if !visit(&e.key, e.rid, &e.slot) {
                            return Ok(());
                        }
                    }
                    link = prev;
                }
            }
            if link == NO_PAGE {
                return Ok(());
            }
            self.pinned.clear();
            id = link;
            pos = if forward { 0 } else { usize::MAX };
        }
    }

    /// Keys of one leaf chosen by walking down at fraction `frac` of each node, with the last key of the previous leaf.
    fn sample_leaf(&mut self, root: PageId, mut frac: f64) -> R<(Option<KeyVals>, Vec<KeyVals>)> {
        let mut id = root;
        loop {
            let f = self.frame(id)?;
            match &f.node {
                Node::Inner { children, .. } => {
                    let n = children.len();
                    let c = ((frac * n as f64) as usize).min(n - 1);
                    frac = frac * n as f64 - c as f64;
                    id = children[c];
                }
                Node::Leaf { entries, prev, .. } => {
                    let keys: Vec<KeyVals> = entries.iter().map(|e| e.key.clone()).collect();
                    let prev = *prev;
                    let mut last = None;
                    if prev != NO_PAGE {
                        if let Node::Leaf { entries, .. } = &self.frame(prev)?.node {
                            last = entries.last().map(|e| e.key.clone());
                        }
                    }
                    return Ok((last, keys));
                }
            }
        }
    }

    fn free_tree(&mut self, root: PageId, has_ovf: bool) -> R<()> {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            self.pinned.clear();
            let kids = match &self.frame(id)?.node {
                Node::Inner { children, leafy, .. } => Some((children.clone(), *leafy)),
                Node::Leaf { .. } => None,
            };
            match kids {
                Some((children, leafy)) => {
                    self.free_id(id);
                    for c in children {
                        if leafy && !has_ovf {
                            self.free_id(c);
                        } else {
                            stack.push(c);
                        }
                    }
                }
                None => {
                    let chains = std::mem::take(&mut self.frames.get_mut(&id).unwrap().chains);
                    for p in chains {
                        self.free_id(p);
                    }
                    self.free_id(id);
                }
            }
        }
        Ok(())
    }

    // ---- checkpoint support
    fn flush_all(&mut self) -> R<()> {
        let ids: Vec<PageId> = self.frames.iter().filter(|(_, f)| f.dirty).map(|(i, _)| *i).collect();
        for id in ids {
            self.flush_frame(id)?;
        }
        if let Some(f) = &self.file {
            f.sync_all().map_err(|e| format!("page file sync failed: {}", e))?;
        }
        Ok(())
    }

    fn image(&self) -> StoreImage {
        StoreImage { table: self.table.iter().map(|e| e.phys).collect() }
    }

    fn commit_checkpoint(&mut self) {
        for e in self.table.iter_mut() {
            if e.phys != NO_BLOCK {
                e.protected = true;
            }
        }
        let pend = std::mem::take(&mut self.pending_free);
        self.free_blocks.extend(pend);
    }
}

fn keys_len(n: &Node) -> usize {
    match n {
        Node::Inner { keys, .. } => keys.len(),
        _ => 0,
    }
}

// ------------------------------------------------------------------- store
/// A page file plus its buffer pool. Cheap to share (`Arc`); all access is serialised by an internal lock.
pub struct PageStore {
    pool: Mutex<Pool>,
    closing: AtomicBool,
}

impl PageStore {
    /// A store that lives entirely in memory.
    pub fn memory() -> Arc<PageStore> {
        Arc::new(PageStore::from_pool(None, false, usize::MAX, StoreImage::default()))
    }

    fn from_pool(file: Option<File>, durable: bool, capacity: usize, image: StoreImage) -> PageStore {
        let nblocks = match &file {
            Some(f) => f.metadata().map(|m| m.len() / PAGE_SIZE as u64).unwrap_or(0),
            None => 0,
        };
        let table: Vec<PEntry> = image.table.iter().map(|&p| PEntry { phys: p, protected: p != NO_BLOCK }).collect();
        let free_ids: Vec<PageId> = (0..table.len()).filter(|&i| table[i].phys == NO_BLOCK).map(|i| i as PageId).rev().collect();
        let used: std::collections::HashSet<u64> = table.iter().filter(|e| e.phys != NO_BLOCK).map(|e| e.phys).collect();
        let free_blocks: Vec<u64> = (0..nblocks).filter(|b| !used.contains(b)).rev().collect();
        PageStore {
            pool: Mutex::new(Pool {
                file,
                durable,
                capacity: capacity.max(8),
                table,
                free_ids,
                nblocks,
                free_blocks,
                pending_free: vec![],
                frames: HashMap::new(),
                order: VecDeque::new(),
                pinned: vec![],
                stats: StoreStats::default(),
                failed: None,
            }),
            closing: AtomicBool::new(false),
        }
    }

    /// Open a page file. `image` is the page table of the last checkpoint (`None` for a new database,
    /// in which case any previous content of the file is discarded). A non-durable store (temporary
    /// tables) spills to the file but is never recovered.
    pub fn open(path: &Path, durable: bool, capacity_pages: usize, image: Option<StoreImage>) -> Result<Arc<PageStore>, SqlError> {
        let io = |e: std::io::Error| SqlError::with(823, 24, 2, format!("I/O error on the page file '{}': {}", path.display(), e));
        let file = OpenOptions::new().create(true).read(true).write(true).truncate(false).open(path).map_err(io)?;
        if image.is_none() {
            file.set_len(0).map_err(io)?;
        }
        Ok(Arc::new(PageStore::from_pool(Some(file), durable, capacity_pages, image.unwrap_or_default())))
    }

    fn run<T: Default>(&self, f: impl FnOnce(&mut Pool) -> R<T>) -> T {
        let mut p = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if p.failed.is_some() {
            return T::default();
        }
        p.pinned.clear();
        let r = f(&mut p);
        p.pinned.clear();
        let t = p.trim();
        match (r, t) {
            (Ok(v), Ok(())) => v,
            (Err(e), _) | (_, Err(e)) => {
                p.failed = Some(e);
                T::default()
            }
        }
    }

    /// The first storage error since the store was opened, if any. Once set, the store refuses all work.
    pub fn failed(&self) -> Option<String> {
        self.pool.lock().unwrap_or_else(|e| e.into_inner()).failed.clone()
    }

    /// Stop freeing pages when trees are dropped (the process is shutting down).
    pub fn set_closing(&self) {
        self.closing.store(true, AtOrd::SeqCst);
    }

    pub fn stats(&self) -> StoreStats {
        let p = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = p.stats;
        s.pool_capacity = if p.capacity == usize::MAX { 0 } else { p.capacity };
        s.pages_cached = p.frames.len();
        s.pages_live = p.table.iter().filter(|e| e.phys != NO_BLOCK).count() + p.frames.iter().filter(|(i, _)| p.table[**i as usize].phys == NO_BLOCK).count();
        s.file_blocks = p.nblocks;
        s.free_blocks = p.free_blocks.len();
        s
    }

    /// Change the cache size (in pages); excess pages are written back and dropped.
    pub fn set_capacity(&self, pages: usize) {
        self.run(|p| {
            p.capacity = pages.max(8);
            Ok(())
        });
    }

    /// Write every dirty page and sync the file; returns the page table to store in the checkpoint.
    pub fn prepare_checkpoint(&self) -> Result<StoreImage, SqlError> {
        let mut p = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = &p.failed {
            return Err(SqlError::with(823, 24, 2, format!("The page store has failed: {}", e)));
        }
        p.pinned.clear();
        if let Err(e) = p.flush_all() {
            p.failed = Some(e.clone());
            return Err(SqlError::with(823, 24, 2, e));
        }
        Ok(p.image())
    }

    /// Call after the checkpoint file naming the image is durable: its blocks become protected and
    /// superseded blocks are released.
    pub fn commit_checkpoint(&self) {
        let mut p = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if p.durable {
            p.commit_checkpoint();
        }
    }
}

// -------------------------------------------------------------------- tree
/// Handle of one B+tree. Dropping the last handle frees the tree's pages.
pub struct Tree {
    pub root: PageId,
    entries: AtomicU64,
    has_ovf: AtomicBool,
    /// Next row id to hand out (heap trees only); shared by every handle of the tree.
    next_id: AtomicU64,
    store: Option<Arc<PageStore>>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct TreeDesc {
    pub root: PageId,
    pub entries: u64,
    pub has_ovf: bool,
    pub next_id: u64,
}

impl Tree {
    pub fn new(store: &Arc<PageStore>) -> Tree {
        let root = store.run(|p| Ok(p.new_tree()));
        Tree { root, entries: AtomicU64::new(0), has_ovf: AtomicBool::new(false), next_id: AtomicU64::new(0), store: Some(store.clone()) }
    }

    pub fn desc(&self) -> TreeDesc {
        TreeDesc { root: self.root, entries: self.entries(), has_ovf: self.has_ovf.load(AtOrd::Relaxed), next_id: self.next_id.load(AtOrd::Relaxed) }
    }

    pub fn from_desc(d: TreeDesc, store: Option<Arc<PageStore>>) -> Tree {
        Tree { root: d.root, entries: AtomicU64::new(d.entries), has_ovf: AtomicBool::new(d.has_ovf), next_id: AtomicU64::new(d.next_id), store }
    }

    pub fn store(&self) -> Option<&Arc<PageStore>> {
        self.store.as_ref()
    }

    /// Reserve and return the next row id.
    pub fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, AtOrd::SeqCst)
    }

    /// Make sure ids up to and including `rid` are never handed out again.
    pub fn reserve_id(&self, rid: u64) {
        self.next_id.fetch_max(rid + 1, AtOrd::SeqCst);
    }

    pub fn attached(&self, store: &Arc<PageStore>) -> Tree {
        Tree::from_desc(self.desc(), Some(store.clone()))
    }

    /// Detach so that dropping this handle frees nothing (used for stale decoded copies).
    pub fn is_attached(&self) -> bool {
        self.store.is_some()
    }

    pub fn entries(&self) -> u64 {
        self.entries.load(AtOrd::Relaxed)
    }

    fn st(&self) -> &Arc<PageStore> {
        self.store.as_ref().expect("tree is not attached to a page store")
    }

    /// Run `f` on the slot stored under `(key, rid)`.
    pub fn with<T>(&self, key: &KeyVals, rid: u64, f: impl FnOnce(Option<&Slot>) -> T) -> T {
        let mut f = Some(f);
        let mut out = None;
        self.st().run(|p| {
            p.bt_peek(self.root, key, rid, &mut |s| {
                if let Some(f) = f.take() {
                    out = Some(f(s));
                }
            })
        });
        match out {
            Some(v) => v,
            None => (f.take().unwrap())(None),
        }
    }

    pub fn put(&self, key: KeyVals, rid: u64, slot: Slot) {
        if postcard::to_allocvec(&slot).map(|v| v.len()).unwrap_or(0) > SPILL_AT {
            self.has_ovf.store(true, AtOrd::Relaxed);
        }
        let new = self.st().run(|p| p.bt_put(self.root, key, rid, slot));
        if new {
            self.entries.fetch_add(1, AtOrd::Relaxed);
        }
    }

    pub fn remove(&self, key: &KeyVals, rid: u64) {
        if self.st().run(|p| p.bt_remove(self.root, key, rid)) {
            self.entries.fetch_sub(1, AtOrd::Relaxed);
        }
    }

    /// See `Pool::bt_scan`.
    pub fn scan(&self, first_true: &dyn Fn(&KeyVals, u64) -> bool, forward: bool, visit: &mut dyn FnMut(&KeyVals, u64, &Slot) -> bool) {
        self.st().run(|p| p.bt_scan(self.root, first_true, forward, visit));
    }

    /// Keys of a leaf at relative position `frac` (0..1) plus the last key of the leaf before it.
    pub fn sample_leaf(&self, frac: f64) -> (Option<KeyVals>, Vec<KeyVals>) {
        self.st().run(|p| p.sample_leaf(self.root, frac))
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        if let Some(s) = &self.store {
            if !s.closing.load(AtOrd::SeqCst) {
                let (root, ovf) = (self.root, self.has_ovf.load(AtOrd::Relaxed));
                s.run(|p| p.free_tree(root, ovf));
            }
        }
    }
}

impl std::fmt::Debug for Tree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tree(root={}, entries={})", self.root, self.entries())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Stamp, Version};
    use crate::types::Value;
    use std::collections::BTreeMap;

    fn slot(n: i32, pad: usize) -> Slot {
        Slot { versions: vec![Version { row: Some(vec![Value::Int(n), Value::Str("x".repeat(pad))]), stamp: Stamp::Ts(1) }] }
    }
    fn key(k: i32) -> KeyVals {
        KeyVals(vec![Value::Int(k)])
    }
    fn dump(t: &Tree) -> Vec<(i32, u64)> {
        let mut out = vec![];
        t.scan(&|_, _| true, true, &mut |k, r, _| {
            if let Value::Int(i) = &k.0[0] {
                out.push((*i, r));
            }
            true
        });
        out
    }

    #[test]
    fn tree_matches_a_btreemap_under_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let store = PageStore::open(&dir.path().join("t.db"), true, 16, None).unwrap();
        let t = Tree::new(&store);
        let mut model: BTreeMap<(i32, u64), usize> = BTreeMap::new();
        let mut seed = 12345u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for step in 0..6000 {
            let k = (rnd() % 1500) as i32;
            let rid = rnd() % 3;
            match rnd() % 4 {
                0 | 1 | 2 => {
                    let pad = if rnd() % 50 == 0 { 3000 } else { (rnd() % 40) as usize };
                    t.put(key(k), rid, slot(k, pad));
                    model.insert((k, rid), pad);
                }
                _ => {
                    t.remove(&key(k), rid);
                    model.remove(&(k, rid));
                }
            }
            if step % 997 == 0 {
                let want: Vec<(i32, u64)> = model.keys().copied().collect();
                assert_eq!(dump(&t), want, "mismatch at step {}", step);
            }
        }
        let want: Vec<(i32, u64)> = model.keys().copied().collect();
        assert_eq!(dump(&t), want);
        assert_eq!(t.entries(), model.len() as u64);
        // contents (including spilled chains) survive eviction
        for ((k, r), pad) in model.iter().take(200) {
            let got = t.with(&key(*k), *r, |s| s.map(|s| s.versions[0].row.clone().unwrap()));
            assert_eq!(format!("{:?}", got.unwrap()[1]), format!("{:?}", Value::Str("x".repeat(*pad))));
        }
        let st = store.stats();
        assert!(st.evictions > 0 && st.writes > 0 && st.reads > 0, "{:?}", st);
        assert!(st.pages_cached <= 16 + 4, "{:?}", st);
        assert!(store.failed().is_none());
        // backward scan agrees
        let mut back = vec![];
        t.scan(&|_, _| false, false, &mut |k, r, _| {
            if let Value::Int(i) = &k.0[0] {
                back.push((*i, r));
            }
            true
        });
        back.reverse();
        assert_eq!(back, want);
    }

    #[test]
    fn dropping_a_tree_frees_its_pages() {
        let store = PageStore::memory();
        let before = store.stats().pages_live;
        {
            let t = Tree::new(&store);
            for i in 0..3000 {
                t.put(key(i), i as u64, slot(i, if i % 100 == 0 { 2500 } else { 5 }));
            }
            assert!(store.stats().pages_live > before + 10);
        }
        assert_eq!(store.stats().pages_live, before);
    }
}
