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
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering as AtOrd};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

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
/// A leaf that shrinks below this after a delete is merged into a neighbour when the two fit together in
/// `MERGE_FIT`, which is well below `NODE_MAX` so a merged leaf does not split again at the next insert.
const MERGE_BELOW: usize = NODE_MAX / 4;
const MERGE_FIT: usize = NODE_MAX * 3 / 4;
/// The most pages moved by one checkpoint when compacting the file.
const COMPACT_MOVES: usize = 4096;
/// Files smaller than this many blocks are never compacted.
const COMPACT_MIN_BLOCKS: u64 = 256;
/// Longest index key (serialised) accepted; SQL Server's limit for non-clustered keys is also 1700.
pub const MAX_KEY_BYTES: usize = 1700;
pub const DEFAULT_POOL_PAGES: usize = 16384; // 128 MiB of pages

type R<T> = Result<T, String>;

/// Read exactly `buf.len()` bytes at `off` without moving a file cursor, so that many threads can use one handle.
fn pread(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            let n = f.seek_read(&mut buf[done..], off + done as u64)?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            done += n;
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        f.read_exact_at(buf, off)
    }
}

/// Write all of `buf` at `off` without moving a file cursor.
fn pwrite(f: &File, buf: &[u8], off: u64) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            let n = f.seek_write(&buf[done..], off + done as u64)?;
            done += n;
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        f.write_all_at(buf, off)
    }
}

pub type TKey = (KeyVals, u64);

fn cmp_tk(k1: &KeyVals, r1: u64, k2: &KeyVals, r2: u64) -> Ordering {
    k1.cmp(k2).then(r1.cmp(&r2))
}

// ------------------------------------------------------------------- nodes
/// What `bt_modify` did.
#[derive(Default, Clone, Copy)]
struct ModOut {
    found: bool,
    created: bool,
    removed: bool,
    big: bool,
}

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
        let klen = postcard::experimental::serialized_size(&key).unwrap_or(0) as u32;
        let slen = postcard::experimental::serialized_size(&slot).unwrap_or(0) as u32;
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
    /// Recently used (clock replacement); atomic so that readers holding the pool shared can set it.
    refbit: AtomicBool,
    /// Overflow pages that back this leaf's on-disk image.
    chains: Vec<PageId>,
    /// A leaf's estimated size, valid while the leaf still has this many entries (inserts and deletes keep it up to
    /// date, so they need not add up every entry; any other change alters the entry count).
    est: Option<(usize, usize)>,
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
    /// Leaf pages merged into a neighbour after deletes.
    pub merges: u64,
    /// Pages moved to lower blocks while compacting the file.
    pub moved: u64,
    /// Blocks cut off the end of the file.
    pub truncated: u64,
}

/// Buffer-cache hits, counted in per-thread slots so that readers on different cores do not fight over one counter.
#[derive(Default)]
struct Hits([HitSlot; 16]);

#[derive(Default)]
#[repr(align(64))]
struct HitSlot(AtomicU64);

impl Hits {
    fn add(&self) {
        thread_local! {
            static SLOT: usize = {
                static NEXT: AtomicU64 = AtomicU64::new(0);
                NEXT.fetch_add(1, AtOrd::Relaxed) as usize
            };
        }
        let i = SLOT.with(|s| *s) % self.0.len();
        self.0[i].0.fetch_add(1, AtOrd::Relaxed);
    }
    fn total(&self) -> u64 {
        self.0.iter().map(|s| s.0.load(AtOrd::Relaxed)).sum()
    }
}

/// The state of the page file that every tree shares: which blocks hold which pages, which are free, and the file
/// itself. It is behind its own mutex, taken for a short step at a time (allocating a page, reading or writing one) by
/// code that already holds the lock of the tree it works on, and never the other way round.
struct Meta {
    file: Option<File>,
    durable: bool,
    table: Vec<PEntry>,
    free_ids: Vec<PageId>,
    nblocks: u64,
    free_blocks: Vec<u64>,
    pending_free: Vec<u64>,
    stats: StoreStats,
}

/// What the trees of a store share (see `PageStore`).
struct Shared {
    meta: Mutex<Meta>,
    /// Is there a page file? (a store without one keeps every page cached, so its reads never need a tree exclusively)
    has_file: bool,
    /// How many frames the trees together may keep, and how many they keep.
    capacity: AtomicUsize,
    cached: AtomicUsize,
    hits: Hits,
    failed: Mutex<Option<String>>,
    broken: AtomicBool,
    /// The page file for reading and writing pages (positionally, outside the `Meta` mutex), and how often it was.
    io: Option<File>,
    reads: AtomicU64,
    writes: AtomicU64,
    /// The cache of every tree, by the page number of its root (which never changes).
    pools: Mutex<HashMap<PageId, Arc<TreePool>>>,
}

/// The cached pages of one tree. Every page belongs to exactly one tree, so the trees need not wait for each other:
/// an operation takes the lock of its own tree only.
type TreePool = RwLock<Pool>;

struct Pool {
    sh: Arc<Shared>,
    frames: HashMap<PageId, Frame>,
    order: VecDeque<PageId>,
    pinned: Vec<PageId>,
}

impl Shared {
    fn meta(&self) -> MutexGuard<'_, Meta> {
        self.meta.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fail(&self, e: String) {
        let mut f = self.failed.lock().unwrap_or_else(|e| e.into_inner());
        if f.is_none() {
            *f = Some(e);
        }
        self.broken.store(true, AtOrd::SeqCst);
    }

    fn pool_list(&self) -> Vec<(PageId, Arc<TreePool>)> {
        let mut v: Vec<(PageId, Arc<TreePool>)> = self.pools.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, p)| (*k, p.clone())).collect();
        v.sort_unstable_by_key(|(k, _)| *k);
        v
    }

    fn over_capacity(&self) -> bool {
        self.cached.load(AtOrd::Relaxed) > self.capacity.load(AtOrd::Relaxed)
    }

    /// Make room by evicting a page of another tree whose lock is free (a tree that is busy is skipped, so this never
    /// waits for a lock while holding one).
    fn evict_elsewhere(&self) -> R<bool> {
        let mut list = self.pool_list();
        // the trees with the most cached pages first
        list.sort_by_key(|(_, p)| std::cmp::Reverse(p.try_read().map(|g| g.frames.len()).unwrap_or(0)));
        for (_, p) in list {
            if let Ok(mut g) = p.try_write() {
                if g.frames.is_empty() {
                    continue;
                }
                if g.evict_one()? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

impl Meta {
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

    /// Choose the block that page `id` is written to next (a page of the last checkpoint is never overwritten).
    fn plan_write(&mut self, id: PageId) -> u64 {
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
        phys
    }

    /// The block page `id` is in.
    fn locate(&self, id: PageId) -> R<u64> {
        let e = *self.table.get(id as usize).ok_or_else(|| format!("page {} does not exist", id))?;
        if e.phys == NO_BLOCK {
            return Err(format!("page {} has no block", id));
        }
        Ok(e.phys)
    }
}

impl Pool {
    fn new(sh: Arc<Shared>) -> Pool {
        Pool { sh, frames: HashMap::new(), order: VecDeque::new(), pinned: vec![] }
    }

    fn evicting(&self) -> bool {
        self.sh.has_file
    }

    // ---- ids and blocks (see `Meta`)
    fn alloc_id(&mut self) -> PageId {
        self.sh.meta().alloc_id()
    }

    fn free_id(&mut self, id: PageId) {
        self.drop_frame(id);
        self.sh.meta().free_id(id);
    }

    /// Write the image of page `id` to its block. Only the choice of the block is made under the `Meta` mutex; the page
    /// is written after it is released (nothing else touches this page: its tree is locked by the caller).
    fn write_bytes(&mut self, id: PageId, bytes: &[u8]) -> R<()> {
        if bytes.len() > BODY {
            return Err(format!("page {} image of {} bytes exceeds the page size", id, bytes.len()));
        }
        let phys = self.sh.meta().plan_write(id);
        let mut buf = vec![0u8; PAGE_SIZE];
        buf[0..4].copy_from_slice(&crc32fast::hash(bytes).to_le_bytes());
        buf[4..8].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        buf[HDR..HDR + bytes.len()].copy_from_slice(bytes);
        let f = self.sh.io.as_ref().ok_or("no page file")?;
        pwrite(f, &buf, phys * PAGE_SIZE as u64).map_err(|e| format!("page write failed: {}", e))?;
        self.sh.writes.fetch_add(1, AtOrd::Relaxed);
        Ok(())
    }

    fn read_bytes(&mut self, id: PageId) -> R<Vec<u8>> {
        let phys = self.sh.meta().locate(id)?;
        let f = self.sh.io.as_ref().ok_or("no page file")?;
        let mut buf = vec![0u8; PAGE_SIZE];
        pread(f, &mut buf, phys * PAGE_SIZE as u64).map_err(|e| format!("page {} read failed: {}", id, e))?;
        let crc = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        if len > BODY || crc32fast::hash(&buf[HDR..HDR + len]) != crc {
            return Err(format!("page {} (block {}) failed its checksum: the database is corrupt", id, phys));
        }
        self.sh.reads.fetch_add(1, AtOrd::Relaxed);
        Ok(buf[HDR..HDR + len].to_vec())
    }

    // ---- frames
    fn put_frame(&mut self, id: PageId, f: Frame) {
        if self.frames.insert(id, f).is_none() {
            self.sh.cached.fetch_add(1, AtOrd::Relaxed);
        }
    }

    fn drop_frame(&mut self, id: PageId) -> Option<Frame> {
        let f = self.frames.remove(&id);
        if f.is_some() {
            self.sh.cached.fetch_sub(1, AtOrd::Relaxed);
        }
        f
    }

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
            self.put_frame(id, Frame { node, dirty: false, refbit: AtomicBool::new(true), chains, est: None });
            self.touch_new(id);
            self.pinned.push(id);
            if self.sh.over_capacity() {
                self.make_room()?;
            }
        } else {
            self.sh.hits.add();
            if !self.pinned.contains(&id) {
                self.pinned.push(id);
            }
        }
        let f = self.frames.get_mut(&id).unwrap();
        *f.refbit.get_mut() = true;
        Ok(f)
    }

    fn new_frame(&mut self, node: Node) -> PageId {
        let id = self.alloc_id();
        self.put_frame(id, Frame { node, dirty: true, refbit: AtomicBool::new(true), chains: vec![], est: None });
        self.touch_new(id);
        self.pinned.push(id);
        id
    }

    /// Evict one page, from this tree if it has one to spare and from another tree otherwise.
    fn make_room(&mut self) -> R<bool> {
        if self.evict_one()? {
            return Ok(true);
        }
        self.sh.evict_elsewhere()
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
                    let r = std::mem::replace(f.refbit.get_mut(), false);
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
            self.drop_frame(id);
            self.sh.meta().stats.evictions += 1;
            return Ok(true);
        }
        Ok(false)
    }

    fn trim(&mut self) -> R<()> {
        while self.evicting() && self.sh.over_capacity() {
            if !self.make_room()? {
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

    // ---- read-only access: a pool held shared can read the pages that are cached (a miss means "retry exclusively")
    fn ro_leaf(&self, root: PageId, first_true: &dyn Fn(&KeyVals, u64) -> bool) -> Option<&Frame> {
        let mut id = root;
        loop {
            let f = self.frames.get(&id)?;
            if !f.refbit.load(AtOrd::Relaxed) {
                f.refbit.store(true, AtOrd::Relaxed);
            }
            self.sh.hits.add();
            match &f.node {
                Node::Leaf { .. } => return Some(f),
                Node::Inner { keys, children, .. } => {
                    id = children[keys.partition_point(|(k, r)| !first_true(k, *r))];
                }
            }
        }
    }

    fn ro_peek(&self, root: PageId, key: &KeyVals, rid: u64, f: &mut dyn FnMut(Option<&Slot>)) -> Option<()> {
        let leaf = self.ro_leaf(root, &|k, r| cmp_tk(k, r, key, rid) == Ordering::Greater)?;
        if let Node::Leaf { entries, .. } = &leaf.node {
            let pos = entries.partition_point(|e| cmp_tk(&e.key, e.rid, key, rid) == Ordering::Less);
            match entries.get(pos) {
                Some(e) if cmp_tk(&e.key, e.rid, key, rid) == Ordering::Equal => f(Some(&e.slot)),
                _ => f(None),
            }
        }
        Some(())
    }

    /// `bt_scan` over a pool that is held shared. Only for a store without a file, where every page is always
    /// cached: a scan that had to stop part-way could not be resumed.
    fn ro_scan(&self, root: PageId, first_true: &dyn Fn(&KeyVals, u64) -> bool, forward: bool, visit: &mut dyn FnMut(&KeyVals, u64, &Slot) -> bool) -> Option<()> {
        let mut leaf = self.ro_leaf(root, first_true)?;
        let mut pos = match &leaf.node {
            Node::Leaf { entries, .. } => entries.partition_point(|e| !first_true(&e.key, e.rid)),
            _ => 0,
        };
        loop {
            let Node::Leaf { entries, prev, next } = &leaf.node else { return None };
            let link;
            if forward {
                for e in &entries[pos.min(entries.len())..] {
                    if !visit(&e.key, e.rid, &e.slot) {
                        return Some(());
                    }
                }
                link = *next;
            } else {
                for e in entries[..pos.min(entries.len())].iter().rev() {
                    if !visit(&e.key, e.rid, &e.slot) {
                        return Some(());
                    }
                }
                link = *prev;
            }
            if link == NO_PAGE {
                return Some(());
            }
            leaf = self.frames.get(&link)?;
            if !leaf.refbit.load(AtOrd::Relaxed) {
                leaf.refbit.store(true, AtOrd::Relaxed);
            }
            pos = if forward { 0 } else { usize::MAX };
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
    fn bt_put(&mut self, root: PageId, key: KeyVals, rid: u64, slot: Slot) -> R<(bool, bool)> {
        let mut path = vec![];
        let leaf = self.find_leaf(root, &|k, r| cmp_tk(k, r, &key, rid) == Ordering::Greater, &mut path)?;
        Ok(self.put_leaf(root, leaf, Some(path), key, rid, slot)?.0)
    }

    /// `bt_put` for many entries (in any order; they are sorted first), keeping the leaf of the previous entry for
    /// the next one when it belongs there too (see `bt_modify_run`). Returns how many entries were new, and whether
    /// any was too big to stay in its leaf.
    fn bt_put_run(&mut self, root: PageId, mut items: Vec<(KeyVals, u64, Slot)>) -> R<(u64, bool)> {
        items.sort_by(|a, b| cmp_tk(&a.0, a.1, &b.0, b.1));
        let (mut new, mut big) = (0u64, false);
        let mut cur: Option<PageId> = None;
        for (key, rid, slot) in items {
            let leaf = match cur {
                Some(l) if self.leaf_covers(l, &key, rid) => l,
                _ => self.find_leaf(root, &|k, r| cmp_tk(k, r, &key, rid) == Ordering::Greater, &mut vec![])?,
            };
            let ((n, b), structural) = self.put_leaf(root, leaf, None, key, rid, slot)?;
            new += n as u64;
            big |= b;
            cur = if structural { None } else { Some(leaf) };
        }
        Ok((new, big))
    }

    /// The work of `bt_put` once the leaf is known (see `modify_leaf`); also says whether the leaf was split.
    fn put_leaf(&mut self, root: PageId, leaf: PageId, path: Option<Vec<(PageId, usize)>>, key: KeyVals, rid: u64, slot: Slot) -> R<((bool, bool), bool)> {
        let entry = LeafEntry::new(key, rid, slot);
        let big = entry.slen as usize > SPILL_AT;
        let size;
        let existed;
        {
            let f = self.frame(leaf)?;
            f.dirty = true;
            let Frame { node, est, .. } = f;
            let entries = match node {
                Node::Leaf { entries, .. } => entries,
                _ => return Err("tree corrupt: expected a leaf".into()),
            };
            let base = match *est {
                Some((n, s)) if n == entries.len() => s,
                _ => entries.iter().map(|e| e.est()).sum::<usize>() + 16,
            };
            let pos = entries.partition_point(|e| cmp_tk(&e.key, e.rid, &entry.key, entry.rid) == Ordering::Less);
            existed = pos < entries.len() && cmp_tk(&entries[pos].key, entries[pos].rid, &entry.key, entry.rid) == Ordering::Equal;
            let added = entry.est();
            if existed {
                size = base - entries[pos].est() + added;
                entries[pos] = entry;
            } else {
                size = base + added;
                entries.insert(pos, entry);
            }
            *est = Some((entries.len(), size));
        }
        if size > NODE_MAX {
            let path = match path {
                Some(p) => p,
                None => {
                    let mut p = vec![];
                    let (k, r) = match &self.frame(leaf)?.node {
                        Node::Leaf { entries, .. } => entries.first().map(|e| (e.key.clone(), e.rid)).ok_or("tree corrupt: an empty leaf is too big")?,
                        _ => return Err("tree corrupt: expected a leaf".into()),
                    };
                    self.find_leaf(root, &|k2, r2| cmp_tk(k2, r2, &k, r) == Ordering::Greater, &mut p)?;
                    p
                }
            };
            self.split_leaf(root, leaf, path)?;
            return Ok(((!existed, big), true));
        }
        Ok(((!existed, big), false))
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

    /// Change the slot stored under `(key, rid)` in place (`create`: start from an empty slot when there is none).
    /// `f` returns whether it changed the slot; a slot left without versions is removed.
    fn bt_modify(&mut self, root: PageId, key: &KeyVals, rid: u64, create: bool, f: &mut dyn FnMut(&mut Slot) -> bool) -> R<ModOut> {
        let mut path = vec![];
        let leaf = self.find_leaf(root, &|k, r| cmp_tk(k, r, key, rid) == Ordering::Greater, &mut path)?;
        self.modify_in_leaf(root, leaf, Some(path), key, rid, create, f)
    }

    /// Does the leaf `leaf` hold the place of `(key, rid)`: is the position inside the range of the keys the leaf
    /// holds (or after them, in the rightmost leaf)? Then a descent from the root would end here.
    fn leaf_covers(&mut self, leaf: PageId, key: &KeyVals, rid: u64) -> bool {
        match self.frame(leaf) {
            Ok(f) => match &f.node {
                Node::Leaf { entries, next, .. } => match (entries.first(), entries.last()) {
                    (Some(a), Some(z)) => cmp_tk(&a.key, a.rid, key, rid) != Ordering::Greater && (*next == NO_PAGE || cmp_tk(key, rid, &z.key, z.rid) != Ordering::Greater),
                    _ => false,
                },
                _ => false,
            },
            Err(_) => false,
        }
    }

    /// `bt_modify` for many row ids of one key (the heap of a table), in the order given. The leaf of the previous
    /// slot is kept and used again for the next one when that slot belongs there too, which saves the descent from
    /// the root for slots that are close together (a statement's rows are). Returns what each call did.
    fn bt_modify_run(&mut self, root: PageId, key: &KeyVals, rids: &[u64], create: bool, f: &mut dyn FnMut(u64, &mut Slot) -> bool) -> R<Vec<ModOut>> {
        let mut outs = Vec::with_capacity(rids.len());
        let mut cur: Option<PageId> = None;
        for &rid in rids {
            let leaf = match cur {
                Some(l) if self.leaf_covers(l, key, rid) => l,
                _ => self.find_leaf(root, &|k, r| cmp_tk(k, r, key, rid) == Ordering::Greater, &mut vec![])?,
            };
            let mut g = |s: &mut Slot| f(rid, s);
            let (out, structural) = self.modify_leaf(root, leaf, None, key, rid, create, &mut g)?;
            cur = if structural { None } else { Some(leaf) };
            outs.push(out);
        }
        Ok(outs)
    }

    #[allow(clippy::too_many_arguments)]
    fn modify_in_leaf(&mut self, root: PageId, leaf: PageId, path: Option<Vec<(PageId, usize)>>, key: &KeyVals, rid: u64, create: bool, f: &mut dyn FnMut(&mut Slot) -> bool) -> R<ModOut> {
        Ok(self.modify_leaf(root, leaf, path, key, rid, create, f)?.0)
    }

    /// The work of `bt_modify` once the leaf is known; `path` (the inner nodes down to it) is worked out again when it
    /// is needed and not given. Also says whether the tree changed shape (a split or a merge).
    #[allow(clippy::too_many_arguments)]
    fn modify_leaf(&mut self, root: PageId, leaf: PageId, path: Option<Vec<(PageId, usize)>>, key: &KeyVals, rid: u64, create: bool, f: &mut dyn FnMut(&mut Slot) -> bool) -> R<(ModOut, bool)> {
        let mut out = ModOut::default();
        let size;
        {
            let fr = self.frame(leaf)?;
            let Frame { node, est, dirty, .. } = fr;
            let Node::Leaf { entries, .. } = node else { return Err("tree corrupt: expected a leaf".into()) };
            let mut base = match *est {
                Some((n, s)) if n == entries.len() => s,
                _ => entries.iter().map(|e| e.est()).sum::<usize>() + 16,
            };
            let pos = entries.partition_point(|e| cmp_tk(&e.key, e.rid, key, rid) == Ordering::Less);
            let existed = pos < entries.len() && cmp_tk(&entries[pos].key, entries[pos].rid, key, rid) == Ordering::Equal;
            if !existed {
                if !create {
                    return Ok((out, false));
                }
                let e = LeafEntry::new(key.clone(), rid, Slot::default());
                base += e.est();
                entries.insert(pos, e);
                out.created = true;
            }
            out.found = true;
            let old_est = entries[pos].est();
            let changed = f(&mut entries[pos].slot);
            if !changed && !out.created {
                *est = Some((entries.len(), base));
                return Ok((out, false));
            }
            *dirty = true;
            if entries[pos].slot.versions.is_empty() {
                entries.remove(pos);
                out.removed = true;
                size = base - old_est;
            } else {
                let e = &mut entries[pos];
                e.slen = postcard::experimental::serialized_size(&e.slot).unwrap_or(0) as u32;
                out.big = e.slen as usize > SPILL_AT;
                size = base - old_est + e.est();
            }
            *est = Some((entries.len(), size));
        }
        let restructure = size > NODE_MAX || (out.removed && leaf != root && size < MERGE_BELOW);
        if restructure {
            let path = match path {
                Some(p) => p,
                None => {
                    let mut p = vec![];
                    self.find_leaf(root, &|k, r| cmp_tk(k, r, key, rid) == Ordering::Greater, &mut p)?;
                    p
                }
            };
            if size > NODE_MAX {
                self.split_leaf(root, leaf, path)?;
            } else {
                self.merge_leaf(root, leaf, path)?;
            }
        }
        Ok((out, restructure))
    }

    fn bt_remove(&mut self, root: PageId, key: &KeyVals, rid: u64) -> R<bool> {
        let mut path = vec![];
        let leaf = self.find_leaf(root, &|k, r| cmp_tk(k, r, key, rid) == Ordering::Greater, &mut path)?;
        let size;
        {
            let f = self.frame(leaf)?;
            let Frame { node, est, dirty, .. } = f;
            let Node::Leaf { entries, .. } = node else { return Ok(false) };
            let pos = entries.partition_point(|e| cmp_tk(&e.key, e.rid, key, rid) == Ordering::Less);
            if !(pos < entries.len() && cmp_tk(&entries[pos].key, entries[pos].rid, key, rid) == Ordering::Equal) {
                return Ok(false);
            }
            let base = match *est {
                Some((n, s)) if n == entries.len() => s,
                _ => entries.iter().map(|e| e.est()).sum::<usize>() + 16,
            };
            let gone = entries.remove(pos);
            *dirty = true;
            size = base - gone.est();
            *est = Some((entries.len(), size));
        }
        if leaf != root && size < MERGE_BELOW {
            self.merge_leaf(root, leaf, path)?;
        }
        Ok(true)
    }

    /// Fold an under-filled leaf into a sibling under the same parent when they fit together, and collapse
    /// inner nodes that are left with a single child. The root page never moves: when the root is left with one
    /// child, the child's contents are copied into it.
    fn merge_leaf(&mut self, root: PageId, leaf: PageId, mut path: Vec<(PageId, usize)>) -> R<()> {
        let Some(&(parent, c)) = path.last() else { return Ok(()) };
        let (left, right, sep_idx) = {
            let f = self.frame(parent)?;
            let Node::Inner { children, leafy, .. } = &f.node else { return Err("tree corrupt: expected an inner node".into()) };
            if !*leafy {
                return Ok(());
            }
            if c + 1 < children.len() {
                (leaf, children[c + 1], c)
            } else if c > 0 {
                (children[c - 1], leaf, c - 1)
            } else {
                return Ok(());
            }
        };
        let size_of = |p: &mut Pool, id: PageId| -> R<usize> {
            match &p.frame(id)?.node {
                Node::Leaf { entries, .. } => Ok(entries.iter().map(|e| e.est()).sum::<usize>() + 16),
                _ => Err("tree corrupt: expected a leaf".to_string()),
            }
        };
        if size_of(self, left)? + size_of(self, right)? > MERGE_FIT {
            return Ok(());
        }
        // move the right leaf's entries into the left one
        let (moved, right_next, right_chains) = {
            let f = self.frame(right)?;
            let chains = std::mem::take(&mut f.chains);
            match &mut f.node {
                Node::Leaf { entries, next, .. } => (std::mem::take(entries), *next, chains),
                _ => return Err("tree corrupt: expected a leaf".into()),
            }
        };
        {
            let f = self.frame(left)?;
            f.dirty = true;
            if let Node::Leaf { entries, next, .. } = &mut f.node {
                entries.extend(moved);
                *next = right_next;
            }
        }
        if right_next != NO_PAGE {
            let f = self.frame(right_next)?;
            f.dirty = true;
            if let Node::Leaf { prev, .. } = &mut f.node {
                *prev = left;
            }
        }
        // the entries live on in the left leaf (whose overflow chains are rewritten when it is flushed)
        for p in right_chains {
            self.free_id(p);
        }
        self.free_id(right);
        {
            let f = self.frame(parent)?;
            f.dirty = true;
            if let Node::Inner { keys, children, .. } = &mut f.node {
                keys.remove(sep_idx);
                children.remove(sep_idx + 1);
            }
        }
        self.sh.meta().stats.merges += 1;
        self.collapse(root, &mut path)
    }

    /// While the node at the end of `path` has a single child, replace it by that child.
    fn collapse(&mut self, root: PageId, path: &mut Vec<(PageId, usize)>) -> R<()> {
        while let Some(&(pid, _)) = path.last() {
            let only = match &self.frame(pid)?.node {
                Node::Inner { keys, children, .. } if keys.is_empty() && children.len() == 1 => children[0],
                _ => return Ok(()),
            };
            if pid == root {
                // copy the child into the root page
                let (node, chains) = {
                    let f = self.frame(only)?;
                    let chains = std::mem::take(&mut f.chains);
                    let node = match &mut f.node {
                        Node::Leaf { entries, .. } => Node::Leaf { entries: std::mem::take(entries), prev: NO_PAGE, next: NO_PAGE },
                        Node::Inner { keys, children, leafy } => Node::Inner { keys: std::mem::take(keys), children: std::mem::take(children), leafy: *leafy },
                    };
                    (node, chains)
                };
                let f = self.frame(root)?;
                f.node = node;
                f.chains = chains;
                f.dirty = true;
                self.free_id(only);
                path.pop();
                // the new root may itself be a single-child inner node
                continue;
            }
            // not the root: the parent's parent points straight at the only child
            if path.len() < 2 {
                return Ok(());
            }
            let (gp, gc) = path[path.len() - 2];
            {
                let f = self.frame(gp)?;
                f.dirty = true;
                if let Node::Inner { children, .. } = &mut f.node {
                    children[gc] = only;
                }
            }
            self.free_id(pid);
            path.pop();
            return Ok(());
        }
        Ok(())
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
    /// Write every dirty page of this tree to the file (the caller syncs the file).
    fn flush_dirty(&mut self) -> R<()> {
        let ids: Vec<PageId> = self.frames.iter().filter(|(_, f)| f.dirty).map(|(i, _)| *i).collect();
        for id in ids {
            self.flush_frame(id)?;
        }
        Ok(())
    }
}

impl Meta {
    fn sync_file(&self) -> R<()> {
        if let Some(f) = &self.file {
            f.sync_all().map_err(|e| format!("page file sync failed: {}", e))?;
        }
        Ok(())
    }

    fn image(&self) -> StoreImage {
        StoreImage { table: self.table.iter().map(|e| e.phys).collect() }
    }

    fn commit_checkpoint(&mut self) -> R<()> {
        for e in self.table.iter_mut() {
            if e.phys != NO_BLOCK {
                e.protected = true;
            }
        }
        let pend = std::mem::take(&mut self.pending_free);
        self.free_blocks.extend(pend);
        self.shrink_file()
    }

    /// Move pages from the end of the file into free blocks nearer its start, so that the end can be cut off.
    /// A moved page gets a new, unprotected block; its old block is released by the checkpoint that follows
    /// (or at once when no checkpoint refers to it), so a crash before that checkpoint still finds every page
    /// where the last checkpoint left it. At most `COMPACT_MOVES` pages are moved per call.
    fn compact(&mut self) -> R<()> {
        if self.file.is_none() || self.nblocks < COMPACT_MIN_BLOCKS {
            return Ok(());
        }
        let live = self.table.iter().filter(|e| e.phys != NO_BLOCK).count() as u64;
        // worth doing only when a good part of the file is free
        if self.free_blocks.len() as u64 * 4 < self.nblocks {
            return Ok(());
        }
        let target = live + live / 16 + 8; // keep a little slack for growth
        if target >= self.nblocks {
            return Ok(());
        }
        let mut high: Vec<(u64, usize)> = self.table.iter().enumerate().filter(|(_, e)| e.phys != NO_BLOCK && e.phys >= target).map(|(i, e)| (e.phys, i)).collect();
        high.sort_unstable_by(|a, b| b.cmp(a));
        let mut low: Vec<u64> = self.free_blocks.iter().copied().filter(|b| *b < target).collect();
        low.sort_unstable();
        // one destination block per page moved
        let n = high.len().min(COMPACT_MOVES).min(low.len());
        let dests: Vec<u64> = low.into_iter().take(n).collect();
        let reserved: std::collections::HashSet<u64> = dests.iter().copied().collect();
        self.free_blocks.retain(|b| !reserved.contains(b));
        for ((phys, id), dest) in high.into_iter().zip(dests) {
            let mut buf = vec![0u8; PAGE_SIZE];
            {
                let f = self.file.as_ref().ok_or("no page file")?;
                pread(f, &mut buf, phys * PAGE_SIZE as u64).map_err(|e| format!("page read failed: {}", e))?;
                pwrite(f, &buf, dest * PAGE_SIZE as u64).map_err(|e| format!("page write failed: {}", e))?;
            }
            let was_protected = self.table[id].protected;
            self.table[id] = PEntry { phys: dest, protected: false };
            if was_protected {
                self.pending_free.push(phys);
            } else {
                self.free_blocks.push(phys);
            }
            self.stats.moved += 1;
        }
        Ok(())
    }

    /// Cut free blocks off the end of the file.
    fn shrink_file(&mut self) -> R<()> {
        let Some(f) = self.file.as_ref() else { return Ok(()) };
        self.free_blocks.sort_unstable_by(|a, b| b.cmp(a)); // descending: the highest block first, `pop` gives the lowest
        let before = self.nblocks;
        let mut free: std::collections::HashSet<u64> = self.free_blocks.iter().copied().collect();
        while self.nblocks > 0 && free.remove(&(self.nblocks - 1)) {
            self.nblocks -= 1;
        }
        if self.nblocks < before {
            self.free_blocks.retain(|b| *b < self.nblocks);
            f.set_len(self.nblocks * PAGE_SIZE as u64).map_err(|e| format!("could not shrink the page file: {}", e))?;
            self.stats.truncated += before - self.nblocks;
        }
        // keep the free list ordered so the lowest block is allocated first
        self.free_blocks.sort_unstable_by(|a, b| b.cmp(a));
        Ok(())
    }
}

fn keys_len(n: &Node) -> usize {
    match n {
        Node::Inner { keys, .. } => keys.len(),
        _ => 0,
    }
}

// ------------------------------------------------------------------- store
/// A page file plus the buffer pool of its trees. Cheap to share (`Arc`). Each tree has a cache of its own pages behind
/// its own lock (so work on different trees, the tables and the indexes, goes on side by side); what they share is the
/// page table and the file (see `Meta`), which are held for one page at a time.
pub struct PageStore {
    sh: Arc<Shared>,
    closing: AtomicBool,
}

impl PageStore {
    /// A store that lives entirely in memory.
    pub fn memory() -> Arc<PageStore> {
        Arc::new(PageStore::from_parts(None, false, usize::MAX, StoreImage::default()))
    }

    fn from_parts(file: Option<File>, durable: bool, capacity: usize, image: StoreImage) -> PageStore {
        let nblocks = match &file {
            Some(f) => f.metadata().map(|m| m.len() / PAGE_SIZE as u64).unwrap_or(0),
            None => 0,
        };
        let table: Vec<PEntry> = image.table.iter().map(|&p| PEntry { phys: p, protected: p != NO_BLOCK }).collect();
        let free_ids: Vec<PageId> = (0..table.len()).filter(|&i| table[i].phys == NO_BLOCK).map(|i| i as PageId).rev().collect();
        let used: std::collections::HashSet<u64> = table.iter().filter(|e| e.phys != NO_BLOCK).map(|e| e.phys).collect();
        let free_blocks: Vec<u64> = (0..nblocks).filter(|b| !used.contains(b)).rev().collect();
        let io = file.as_ref().and_then(|f| f.try_clone().ok());
        PageStore {
            sh: Arc::new(Shared {
                io,
                reads: AtomicU64::new(0),
                writes: AtomicU64::new(0),
                has_file: file.is_some(),
                meta: Mutex::new(Meta { file, durable, table, free_ids, nblocks, free_blocks, pending_free: vec![], stats: StoreStats::default() }),
                capacity: AtomicUsize::new(capacity.max(8)),
                cached: AtomicUsize::new(0),
                hits: Hits::default(),
                failed: Mutex::new(None),
                broken: AtomicBool::new(false),
                pools: Mutex::new(HashMap::new()),
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
        Ok(Arc::new(PageStore::from_parts(Some(file), durable, capacity_pages, image.unwrap_or_default())))
    }

    /// The cache of the tree whose root is `root` (made when there is none yet).
    fn pool_for(&self, root: PageId) -> Arc<TreePool> {
        let mut m = self.sh.pools.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(root).or_insert_with(|| Arc::new(RwLock::new(Pool::new(self.sh.clone())))).clone()
    }

    /// Run `f` on the cache of one tree, which is held for the whole call.
    fn run_in<T: Default>(&self, pool: &TreePool, f: impl FnOnce(&mut Pool) -> R<T>) -> T {
        let mut p = pool.write().unwrap_or_else(|e| e.into_inner());
        if self.sh.broken.load(AtOrd::Relaxed) {
            return T::default();
        }
        p.pinned.clear();
        let r = f(&mut p);
        p.pinned.clear();
        let t = p.trim();
        match (r, t) {
            (Ok(v), Ok(())) => v,
            (Err(e), _) | (_, Err(e)) => {
                self.sh.fail(e);
                T::default()
            }
        }
    }

    /// Run `f` with the cache of one tree held shared (so readers do not wait for one another). `f` returns `None` when
    /// it needs a page that is not cached; the caller then retries through `run_in`.
    fn read_in<T: Default>(&self, pool: &TreePool, f: impl FnOnce(&Pool) -> Option<T>) -> Option<T> {
        let p = pool.read().unwrap_or_else(|e| e.into_inner());
        if self.sh.broken.load(AtOrd::Relaxed) {
            return Some(T::default());
        }
        f(&p)
    }

    /// The first storage error since the store was opened, if any. Once set, the store refuses all work.
    pub fn failed(&self) -> Option<String> {
        self.sh.failed.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Stop freeing pages when trees are dropped (the process is shutting down).
    pub fn set_closing(&self) {
        self.closing.store(true, AtOrd::SeqCst);
    }

    pub fn stats(&self) -> StoreStats {
        // the pages that are cached and were never written have no block yet (found before the page table is taken)
        let mut unwritten: Vec<PageId> = vec![];
        for (_, p) in self.sh.pool_list() {
            let g = p.read().unwrap_or_else(|e| e.into_inner());
            unwritten.extend(g.frames.keys().copied());
        }
        let m = self.sh.meta();
        let mut s = m.stats;
        s.hits = self.sh.hits.total();
        s.reads = self.sh.reads.load(AtOrd::Relaxed);
        s.writes = self.sh.writes.load(AtOrd::Relaxed);
        let cap = self.sh.capacity.load(AtOrd::Relaxed);
        s.pool_capacity = if cap == usize::MAX { 0 } else { cap };
        s.pages_cached = self.sh.cached.load(AtOrd::Relaxed);
        s.pages_live = m.table.iter().filter(|e| e.phys != NO_BLOCK).count() + unwritten.iter().filter(|i| m.table.get(**i as usize).is_some_and(|e| e.phys == NO_BLOCK)).count();
        s.file_blocks = m.nblocks;
        s.free_blocks = m.free_blocks.len();
        s
    }

    /// Change the cache size (in pages); excess pages are written back and dropped.
    pub fn set_capacity(&self, pages: usize) {
        self.sh.capacity.store(pages.max(8), AtOrd::Relaxed);
        for (_, p) in self.sh.pool_list() {
            self.run_in(&p, |_| Ok(()));
        }
    }

    /// Write every dirty page and sync the file; returns the page table to store in the checkpoint.
    pub fn prepare_checkpoint(&self) -> Result<StoreImage, SqlError> {
        let fail = |e: String| SqlError::with(823, 24, 2, format!("The page store has failed: {}", e));
        if let Some(e) = self.failed() {
            return Err(fail(e));
        }
        // every tree is held (in a fixed order) so that the page table matches the pages that are written
        let list = self.sh.pool_list();
        let mut guards: Vec<std::sync::RwLockWriteGuard<'_, Pool>> = list.iter().map(|(_, p)| p.write().unwrap_or_else(|e| e.into_inner())).collect();
        for g in guards.iter_mut() {
            g.pinned.clear();
        }
        // compaction is only for the durable store; a failure to compact is a failure of the store
        let durable = self.sh.meta().durable;
        let mut r = if durable { self.sh.meta().compact() } else { Ok(()) };
        if r.is_ok() {
            for g in guards.iter_mut() {
                r = g.flush_dirty();
                if r.is_err() {
                    break;
                }
            }
        }
        let r = r.and_then(|_| self.sh.meta().sync_file());
        if let Err(e) = r {
            self.sh.fail(e.clone());
            return Err(SqlError::with(823, 24, 2, e));
        }
        Ok(self.sh.meta().image())
    }

    /// Call after the checkpoint file naming the image is durable: its blocks become protected and
    /// superseded blocks are released.
    pub fn commit_checkpoint(&self) {
        let mut m = self.sh.meta();
        if m.durable {
            if let Err(e) = m.commit_checkpoint() {
                drop(m);
                self.sh.fail(e);
            }
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
    /// The cache of this tree's pages (shared by every handle of the tree).
    pool: Option<Arc<TreePool>>,
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
        let pool = Arc::new(RwLock::new(Pool::new(store.sh.clone())));
        let root = store.run_in(&pool, |p| Ok(p.new_tree()));
        store.sh.pools.lock().unwrap_or_else(|e| e.into_inner()).insert(root, pool.clone());
        Tree { root, entries: AtomicU64::new(0), has_ovf: AtomicBool::new(false), next_id: AtomicU64::new(0), store: Some(store.clone()), pool: Some(pool) }
    }

    pub fn desc(&self) -> TreeDesc {
        TreeDesc { root: self.root, entries: self.entries(), has_ovf: self.has_ovf.load(AtOrd::Relaxed), next_id: self.next_id.load(AtOrd::Relaxed) }
    }

    pub fn from_desc(d: TreeDesc, store: Option<Arc<PageStore>>) -> Tree {
        let pool = store.as_ref().map(|s| s.pool_for(d.root));
        Tree { root: d.root, entries: AtomicU64::new(d.entries), has_ovf: AtomicBool::new(d.has_ovf), next_id: AtomicU64::new(d.next_id), store, pool }
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

    /// Run `f` on this tree's cache, held exclusively.
    fn run<T: Default>(&self, f: impl FnOnce(&mut Pool) -> R<T>) -> T {
        self.st().run_in(self.pool.as_ref().expect("tree is not attached to a page store"), f)
    }

    /// Run `f` on this tree's cache, held shared (see `PageStore::read_in`).
    fn rd<T: Default>(&self, f: impl FnOnce(&Pool) -> Option<T>) -> Option<T> {
        self.st().read_in(self.pool.as_ref().expect("tree is not attached to a page store"), f)
    }

    fn st(&self) -> &Arc<PageStore> {
        self.store.as_ref().expect("tree is not attached to a page store")
    }

    /// Run `f` on the slot stored under `(key, rid)`.
    pub fn with<T>(&self, key: &KeyVals, rid: u64, f: impl FnOnce(Option<&Slot>) -> T) -> T {
        let mut f = Some(f);
        let mut out = None;
        let hit = self.rd(|p| {
            p.ro_peek(self.root, key, rid, &mut |s| {
                if let Some(f) = f.take() {
                    out = Some(f(s));
                }
            })
        });
        if hit.is_none() {
            self.run(|p| {
                p.bt_peek(self.root, key, rid, &mut |s| {
                    if let Some(f) = f.take() {
                        out = Some(f(s));
                    }
                })
            });
        }
        match out {
            Some(v) => v,
            None => (f.take().unwrap())(None),
        }
    }

    /// `put` for many entries (see `Pool::bt_put_run`).
    pub fn put_run(&self, items: Vec<(KeyVals, u64, Slot)>) {
        let mut items = items;
        while !items.is_empty() {
            let rest = if items.len() > 512 { items.split_off(512) } else { vec![] };
            let chunk = std::mem::replace(&mut items, rest);
            let (new, big) = self.run(|p| p.bt_put_run(self.root, chunk));
            if big {
                self.has_ovf.store(true, AtOrd::Relaxed);
            }
            self.entries.fetch_add(new, AtOrd::Relaxed);
        }
    }

    pub fn put(&self, key: KeyVals, rid: u64, slot: Slot) {
        let (new, big) = self.run(|p| p.bt_put(self.root, key, rid, slot));
        if big {
            self.has_ovf.store(true, AtOrd::Relaxed);
        }
        if new {
            self.entries.fetch_add(1, AtOrd::Relaxed);
        }
    }

    /// Change a slot in place (see `bt_modify`); returns whether the slot existed (or was created).
    pub fn modify(&self, key: &KeyVals, rid: u64, create: bool, mut f: impl FnMut(&mut Slot) -> bool) -> bool {
        let out = self.run(|p| p.bt_modify(self.root, key, rid, create, &mut f));
        if out.big {
            self.has_ovf.store(true, AtOrd::Relaxed);
        }
        if out.created && !out.removed {
            self.entries.fetch_add(1, AtOrd::Relaxed);
        } else if out.removed && !out.created {
            self.entries.fetch_sub(1, AtOrd::Relaxed);
        }
        out.found
    }

    /// `modify` for many slots of one key (see `Pool::bt_modify_run`): `f` is told which row id it is called for.
    /// Returns, per row id, whether the slot existed (or was created).
    pub fn modify_run(&self, key: &KeyVals, rids: &[u64], create: bool, mut f: impl FnMut(u64, &mut Slot) -> bool) -> Vec<bool> {
        let mut found = Vec::with_capacity(rids.len());
        // in chunks, so that one big statement does not keep the page pool from other sessions for long
        for chunk in rids.chunks(512) {
            let outs = self.run(|p| p.bt_modify_run(self.root, key, chunk, create, &mut f));
            for out in outs {
                if out.big {
                    self.has_ovf.store(true, AtOrd::Relaxed);
                }
                if out.created && !out.removed {
                    self.entries.fetch_add(1, AtOrd::Relaxed);
                } else if out.removed && !out.created {
                    self.entries.fetch_sub(1, AtOrd::Relaxed);
                }
                found.push(out.found);
            }
        }
        found
    }

    pub fn remove(&self, key: &KeyVals, rid: u64) {
        if self.run(|p| p.bt_remove(self.root, key, rid)) {
            self.entries.fetch_sub(1, AtOrd::Relaxed);
        }
    }

    /// See `Pool::bt_scan`.
    pub fn scan(&self, first_true: &dyn Fn(&KeyVals, u64) -> bool, forward: bool, visit: &mut dyn FnMut(&KeyVals, u64, &Slot) -> bool) {
        if !self.st().sh.has_file && self.rd(|p| p.ro_scan(self.root, first_true, forward, &mut *visit)).is_some() {
            return;
        }
        self.run(|p| p.bt_scan(self.root, first_true, forward, visit));
    }

    /// Keys of a leaf at relative position `frac` (0..1) plus the last key of the leaf before it.
    pub fn sample_leaf(&self, frac: f64) -> (Option<KeyVals>, Vec<KeyVals>) {
        self.run(|p| p.sample_leaf(self.root, frac))
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        if let Some(s) = &self.store {
            if !s.closing.load(AtOrd::SeqCst) {
                let (root, ovf) = (self.root, self.has_ovf.load(AtOrd::Relaxed));
                self.run(|p| p.free_tree(root, ovf));
                // the page numbers are free for another tree now: its cache must not be found under this one's root
                let mut pools = s.sh.pools.lock().unwrap_or_else(|e| e.into_inner());
                if pools.get(&root).is_some_and(|p| self.pool.as_ref().is_some_and(|q| Arc::ptr_eq(p, q))) {
                    pools.remove(&root);
                }
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

    /// The batch calls give the same tree as the single ones: random batches of `modify_run` (creating, replacing and
    /// emptying slots, so leaves split and merge in the middle of a batch) and of `put_run`, against a map, with a
    /// cache so small that the pages are evicted all the time.
    #[test]
    fn batch_modify_and_put_match_a_btreemap_under_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let store = PageStore::open(&dir.path().join("b.db"), true, 16, None).unwrap();
        let heap = Tree::new(&store);
        let index = Tree::new(&store);
        let mut seed = 777u64;
        let mut hmodel: BTreeMap<u64, usize> = BTreeMap::new();
        let mut imodel: BTreeMap<(i32, u64), ()> = BTreeMap::new();
        let hk = key(0);
        let heap_dump = |t: &Tree| {
            let mut out = vec![];
            t.scan(&|_, _| true, true, &mut |_, r, s| {
                if let Some(Value::Str(p)) = s.versions.first().and_then(|v| v.row.as_ref()).map(|r| r[1].clone()) {
                    out.push((r, p.len()));
                }
                true
            });
            out
        };
        for step in 0..400 {
            // a batch of row ids, in no particular order, some of them repeated
            let n = 1 + (lcg(&mut seed) % 60) as usize;
            let base = lcg(&mut seed) % 900;
            let mut rids: Vec<u64> = (0..n).map(|_| base + lcg(&mut seed) % 200).collect();
            if step % 3 == 0 {
                rids.sort_unstable();
                rids.dedup();
            }
            match lcg(&mut seed) % 3 {
                0 | 1 => {
                    // create or replace; some slots are big enough to fill a leaf on their own
                    let create = step % 5 != 4;
                    let pad_of = |rid: u64| if rid % 41 == 0 { 3000 } else { ((rid * 7 + step as u64) % 60) as usize };
                    let found = heap.modify_run(&hk, &rids, create, |rid, s| {
                        *s = slot(rid as i32, pad_of(rid));
                        true
                    });
                    for (i, rid) in rids.iter().enumerate() {
                        let existed = hmodel.contains_key(rid);
                        assert_eq!(found[i], existed || create, "step {}", step);
                        if existed || create {
                            hmodel.insert(*rid, pad_of(*rid));
                        }
                    }
                }
                _ => {
                    // empty the slots: they are removed (and the leaves they were in merge)
                    let found = heap.modify_run(&hk, &rids, false, |_, s| {
                        s.versions.clear();
                        true
                    });
                    for (i, rid) in rids.iter().enumerate() {
                        assert_eq!(found[i], hmodel.contains_key(rid), "step {}", step);
                        hmodel.remove(rid);
                    }
                }
            }
            // index entries: a run of puts (some of them already there)
            let items: Vec<(KeyVals, u64, Slot)> = (0..n)
                .map(|_| {
                    let (k, r) = ((lcg(&mut seed) % 300) as i32, lcg(&mut seed) % 4);
                    imodel.insert((k, r), ());
                    (key(k), r, Slot::default())
                })
                .collect();
            index.put_run(items);
            if step % 20 == 0 {
                assert_eq!(heap_dump(&heap), hmodel.iter().map(|(r, p)| (*r, *p)).collect::<Vec<_>>(), "heap at step {}", step);
                assert_eq!(dump(&index), imodel.keys().copied().collect::<Vec<_>>(), "index at step {}", step);
            }
        }
        assert_eq!(heap_dump(&heap), hmodel.iter().map(|(r, p)| (*r, *p)).collect::<Vec<_>>());
        assert_eq!(dump(&index), imodel.keys().copied().collect::<Vec<_>>());
        assert_eq!(heap.entries(), hmodel.len() as u64);
        assert_eq!(index.entries(), imodel.len() as u64);
        assert!(store.failed().is_none());
        let st = store.stats();
        // the leaves did merge and split in the middle of batches
        assert!(st.evictions > 0 && st.merges > 0, "{:?}", st);
    }

    /// Trees have a cache each but share the pool's size: threads that each work on a tree of their own, with a cache
    /// far too small for all of them (so pages of other trees are evicted), keep every tree intact, a checkpoint in
    /// the middle sees a consistent store, and the trees together stay within the pool.
    #[test]
    fn trees_on_different_threads_share_one_small_pool() {
        let dir = tempfile::tempdir().unwrap();
        let store = PageStore::open(&dir.path().join("m.db"), true, 24, None).unwrap();
        let trees: Vec<Arc<Tree>> = (0..6).map(|_| Arc::new(Tree::new(&store))).collect();
        let hs: Vec<_> = trees
            .iter()
            .enumerate()
            .map(|(n, t)| {
                let t = t.clone();
                std::thread::spawn(move || {
                    let mut seed = 1000 + n as u64;
                    let mut model: BTreeMap<(i32, u64), usize> = BTreeMap::new();
                    for step in 0..1500 {
                        let k = (lcg(&mut seed) % 700) as i32;
                        if lcg(&mut seed) % 4 == 0 {
                            t.remove(&key(k), 0);
                            model.remove(&(k, 0));
                        } else {
                            let pad = if lcg(&mut seed) % 60 == 0 { 2500 } else { (lcg(&mut seed) % 40) as usize };
                            t.put(key(k), 0, slot(k, pad));
                            model.insert((k, 0), pad);
                        }
                        if step % 300 == 0 {
                            let want: Vec<(i32, u64)> = model.keys().copied().collect();
                            assert_eq!(dump(&t), want, "tree {} at step {}", n, step);
                        }
                    }
                    model
                })
            })
            .collect();
        // checkpoints while the threads work
        for _ in 0..5 {
            assert!(store.prepare_checkpoint().is_ok());
            store.commit_checkpoint();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        for (n, h) in hs.into_iter().enumerate() {
            let model = h.join().unwrap();
            let want: Vec<(i32, u64)> = model.keys().copied().collect();
            assert_eq!(dump(&trees[n]), want, "tree {}", n);
            assert_eq!(trees[n].entries(), model.len() as u64);
        }
        assert!(store.failed().is_none(), "{:?}", store.failed());
        let st = store.stats();
        assert!(st.evictions > 0, "{:?}", st);
        // every tree keeps at least the pages of one operation, so the pool may be exceeded by a few per tree
        assert!(st.pages_cached <= 24 + 6 * 4, "{:?}", st);
        // a checkpoint and a reopen find everything
        let image = store.prepare_checkpoint().unwrap();
        store.commit_checkpoint();
        let descs: Vec<TreeDesc> = trees.iter().map(|t| t.desc()).collect();
        let dumps: Vec<Vec<(i32, u64)>> = trees.iter().map(|t| dump(t)).collect();
        drop(trees);
        store.set_closing();
        drop(store);
        let store2 = PageStore::open(&dir.path().join("m.db"), true, 24, Some(image)).unwrap();
        for (d, want) in descs.into_iter().zip(dumps) {
            let t = Tree::from_desc(d, Some(store2.clone()));
            assert_eq!(dump(&t), want);
        }
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

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *seed >> 33
    }

    #[test]
    fn deleting_most_entries_merges_leaves_and_collapses_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let store = PageStore::open(&dir.path().join("m.db"), true, 24, None).unwrap();
        let t = Tree::new(&store);
        let n = 4000;
        for k in 0..n {
            t.put(key(k), 0, slot(k, 60));
        }
        let full = store.stats().pages_live;
        assert!(full > 30, "{full}");
        // delete in pseudo-random order, keeping every 20th entry
        let mut order: Vec<i32> = (0..n).filter(|k| k % 20 != 0).collect();
        let mut seed = 7u64;
        for i in (1..order.len()).rev() {
            let j = (lcg(&mut seed) as usize) % (i + 1);
            order.swap(i, j);
        }
        for (i, k) in order.iter().enumerate() {
            t.remove(&key(*k), 0);
            if i % 997 == 0 {
                assert!(store.failed().is_none());
            }
        }
        let want: Vec<(i32, u64)> = (0..n).filter(|k| k % 20 == 0).map(|k| (k, 0)).collect();
        assert_eq!(dump(&t), want);
        let st = store.stats();
        assert!(st.merges > 20, "leaves should have been merged: {:?}", st);
        assert!(st.pages_live * 3 < full, "pages should have been freed: {} of {}", st.pages_live, full);
        // backward scans and seeks still agree, and the tree accepts new entries
        let mut back = vec![];
        t.scan(&|_, _| false, false, &mut |k, r, _| {
            if let Value::Int(i) = &k.0[0] {
                back.push((*i, r));
            }
            true
        });
        back.reverse();
        assert_eq!(back, want);
        assert!(t.with(&key(40), 0, |s| s.is_some()));
        assert!(t.with(&key(41), 0, |s| s.is_none()));
        for k in 0..n {
            t.put(key(k), 1, slot(k, 10));
        }
        assert_eq!(t.entries() as usize, want.len() + n as usize);
        // delete everything: the root is a single empty leaf again, with no other pages left
        for k in 0..n {
            t.remove(&key(k), 1);
            t.remove(&key(k), 0);
        }
        assert_eq!(dump(&t), vec![]);
        assert_eq!(store.stats().pages_live, 1, "{:?}", store.stats());
        t.put(key(5), 0, slot(5, 1));
        assert_eq!(dump(&t), vec![(5, 0)]);
        assert!(store.failed().is_none());
    }

    #[test]
    fn the_page_file_is_compacted_and_truncated_after_big_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.db");
        let store = PageStore::open(&path, true, 64, None).unwrap();
        let t = Tree::new(&store);
        let n = 6000;
        for k in 0..n {
            t.put(key(k), 0, slot(k, 400));
        }
        let checkpoint = |store: &Arc<PageStore>| {
            store.prepare_checkpoint().unwrap();
            store.commit_checkpoint();
        };
        checkpoint(&store);
        let big = std::fs::metadata(&path).unwrap().len();
        assert!(big > 256 * PAGE_SIZE as u64, "{big}");
        // delete 95% in a scattered pattern, so free blocks are spread through the file
        for k in 0..n {
            if k % 20 != 0 {
                t.remove(&key(k), 0);
            }
        }
        for _ in 0..3 {
            checkpoint(&store);
        }
        let small = std::fs::metadata(&path).unwrap().len();
        let st = store.stats();
        assert!(small * 3 < big, "file went from {big} to {small}: {:?}", st);
        assert!(st.moved > 0 && st.truncated > 0, "{:?}", st);
        assert_eq!(st.file_blocks * PAGE_SIZE as u64, small);
        // everything that is left reads back correctly from the compacted file
        let want: Vec<(i32, u64)> = (0..n).filter(|k| k % 20 == 0).map(|k| (k, 0)).collect();
        assert_eq!(dump(&t), want);
        // and the freed room is reused rather than growing the file
        for k in 0..2000 {
            t.put(key(100_000 + k), 0, slot(k, 400));
        }
        checkpoint(&store);
        assert!(std::fs::metadata(&path).unwrap().len() < big);
        assert!(store.failed().is_none());
    }
}
