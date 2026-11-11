//! The `hierarchyid` type: the binary form SQL Server stores, and its methods.
//!
//! A node is a path of labels from the root: `/` is the root (no bytes at all), `/1/` its first child, `/1/2/` the second child
//! of that. The stored form is the labels written one after another as a prefix code, then zero bits up to a whole byte, and it
//! is built so that comparing the bytes compares the nodes in tree order (a parent before its children, siblings by label), so
//! `ORDER BY`, `=`, `<` and indexes work on the bytes as they are.
//!
//! One label (a non-negative whole number) is written as a prefix that tells how many bits follow, those bits, and a final `1`:
//!
//! | prefix | bits | labels |
//! | --- | --- | --- |
//! | `01` | 2 | 0 – 3 |
//! | `100` | 2 | 4 – 7 |
//! | `101` | 3 | 8 – 15 |
//! | `110` | 8 | 16 – 271 |
//!
//! so `/1/` is `0101 1000` = `0x58`, `/1/1/` is `0x5AC0` and `/3/1/24/` is `0x7AF044`. All 301 values of the AdventureWorks
//! sample (`Employee.OrganizationNode`, `Document.DocumentNode`) decode with this table, every parent is there, and the byte
//! order is the tree order.
//!
//! **What is not supported**, because it has not been seen in a backup and the form cannot be worked out from the rest:
//! labels above 271, negative labels, and the fractional labels (`/1.1/`) that SQL Server uses to insert a node between two
//! siblings with no room between them. They are errors, not guesses; `GetDescendant` needs a free label between its two
//! arguments (or none), and says so when there is none.

pub type Path = Vec<u64>;

const RANGES: [(u32, u32, u32, u64); 4] = [
    // (prefix, prefix bits, value bits, first label)
    (0b01, 2, 2, 0),
    (0b100, 3, 2, 4),
    (0b101, 3, 3, 8),
    (0b110, 3, 8, 16),
];

/// The highest label that can be written.
pub const MAX_LABEL: u64 = 271;

fn push(bits: &mut Vec<bool>, v: u64, n: u32) {
    for k in (0..n).rev() {
        bits.push(v >> k & 1 == 1);
    }
}

/// The stored form of a node.
pub fn encode(path: &[u64]) -> Result<Vec<u8>, String> {
    let mut bits: Vec<bool> = vec![];
    for &l in path {
        let (p, pn, w, base) = RANGES.iter().copied().find(|&(_, _, w, base)| l >= base && l < base + (1u64 << w)).ok_or_else(|| format!("the label {} cannot be stored (labels go up to {})", l, MAX_LABEL))?;
        push(&mut bits, p as u64, pn);
        push(&mut bits, l - base, w);
        bits.push(true);
    }
    let mut out = vec![0u8; bits.len().div_ceil(8)];
    for (i, b) in bits.iter().enumerate() {
        if *b {
            out[i / 8] |= 0x80 >> (i % 8);
        }
    }
    Ok(out)
}

/// The path of a stored node.
pub fn decode(bytes: &[u8]) -> Result<Path, String> {
    let bit = |i: usize| bytes[i / 8] & (0x80 >> (i % 8)) != 0;
    let total = bytes.len() * 8;
    let take = |i: usize, n: u32| -> Option<u64> {
        if i + n as usize > total {
            return None;
        }
        Some((0..n as usize).fold(0u64, |a, k| a << 1 | bit(i + k) as u64))
    };
    let mut i = 0;
    let mut path = vec![];
    while i < total {
        // zero bits up to the end of the last byte are padding
        if total - i < 8 && (i..total).all(|k| !bit(k)) {
            break;
        }
        let Some(&(_, pn, w, base)) = RANGES.iter().find(|&&(p, pn, _, _)| take(i, pn) == Some(p as u64)) else {
            return Err("the bytes are not a hierarchyid that this server can read (negative or very large labels)".into());
        };
        let v = take(i + pn as usize, w).ok_or("the hierarchyid is cut short")?;
        if take(i + (pn + w) as usize, 1) != Some(1) {
            return Err("the bytes are not a hierarchyid that this server can read (a fractional label)".into());
        }
        path.push(base + v);
        i += (pn + w) as usize + 1;
    }
    Ok(path)
}

/// `/`, `/1/`, `/1/2/`.
pub fn to_string(path: &[u64]) -> String {
    let mut s = String::from("/");
    for l in path {
        s.push_str(&l.to_string());
        s.push('/');
    }
    s
}

pub fn parse(text: &str) -> Result<Path, String> {
    let t = text.trim();
    let bad = || format!("'{}' is not a valid string representation of a hierarchyid node", text);
    if !t.starts_with('/') || !t.ends_with('/') {
        return Err(bad());
    }
    if t == "/" {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for part in t[1..t.len() - 1].split('/') {
        if part.contains('.') || part.starts_with('-') {
            return Err(format!("{}: fractional and negative labels are not supported", bad()));
        }
        let l: u64 = part.parse().map_err(|_| bad())?;
        if l > MAX_LABEL {
            return Err(format!("{}: labels go up to {}", bad(), MAX_LABEL));
        }
        out.push(l);
    }
    Ok(out)
}

pub fn level(path: &[u64]) -> i32 {
    path.len() as i32
}

/// The ancestor `n` levels up; none if there are not that many.
pub fn ancestor(path: &[u64], n: i64) -> Option<Path> {
    if n < 0 || n as usize > path.len() {
        return None;
    }
    Some(path[..path.len() - n as usize].to_vec())
}

/// Is `path` below `parent`, or `parent` itself?
pub fn is_descendant_of(path: &[u64], parent: &[u64]) -> bool {
    path.starts_with(parent)
}

/// `path` with its ancestor `old` replaced by `new`; none if `path` is not below `old`.
pub fn reparent(path: &[u64], old: &[u64], new: &[u64]) -> Option<Path> {
    if !path.starts_with(old) {
        return None;
    }
    let mut p = new.to_vec();
    p.extend_from_slice(&path[old.len()..]);
    Some(p)
}

/// A new child of `parent`: the first one (label 1), after `child1`, before `child2`, or between them.
pub fn descendant(parent: &[u64], child1: Option<&[u64]>, child2: Option<&[u64]>) -> Result<Path, String> {
    let label = |c: &[u64]| -> Result<u64, String> {
        if c.len() == parent.len() + 1 && c.starts_with(parent) {
            Ok(c[c.len() - 1])
        } else {
            Err("the argument of GetDescendant is not a child of the node".to_string())
        }
    };
    let l = match (child1, child2) {
        (None, None) => 1,
        (Some(a), None) => label(a)? + 1,
        (None, Some(b)) => {
            let b = label(b)?;
            if b == 0 {
                return Err("there is no label before 0 (negative labels are not supported)".into());
            }
            b - 1
        }
        (Some(a), Some(b)) => {
            let (a, b) = (label(a)?, label(b)?);
            if a >= b {
                return Err("the first argument of GetDescendant must come before the second".into());
            }
            if b - a < 2 {
                return Err(format!("there is no label between {} and {} (fractional labels are not supported)", a, b));
            }
            a + 1
        }
    };
    if l > MAX_LABEL {
        return Err(format!("the label {} cannot be stored (labels go up to {})", l, MAX_LABEL));
    }
    let mut p = parent.to_vec();
    p.push(l);
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stored_form_of_nodes_seen_in_adventureworks() {
        for (s, hex) in [
            ("/", ""),
            ("/1/", "58"),
            ("/2/", "68"),
            ("/3/", "78"),
            ("/1/1/", "5AC0"),
            ("/1/2/", "5B40"),
            ("/1/4/", "5C20"),
            ("/2/5/", "6C60"),
            ("/2/8/", "6D10"),
            ("/3/1/1/", "7AD6"),
            ("/3/1/4/", "7AE1"),
            ("/3/1/1/4/", "7AD708"),
            ("/3/1/8/", "7AE880"),
            ("/3/1/24/", "7AF044"),
            ("/3/1/24/1/", "7AF04560"),
        ] {
            let p = parse(s).unwrap();
            let b = encode(&p).unwrap();
            let h: String = b.iter().map(|x| format!("{:02X}", x)).collect();
            assert_eq!(h, hex, "{}", s);
            assert_eq!(decode(&b).unwrap(), p, "{}", s);
            assert_eq!(to_string(&p), s);
        }
    }

    #[test]
    fn the_byte_order_is_the_tree_order() {
        let mut nodes: Vec<Path> = vec![vec![]];
        for a in [1u64, 2, 3, 4, 5, 7, 8, 12, 15, 16, 24, 100, 271] {
            nodes.push(vec![a]);
            for b in [1u64, 2, 9, 16, 200] {
                nodes.push(vec![a, b]);
                nodes.push(vec![a, b, 1]);
            }
        }
        let mut by_bytes = nodes.clone();
        by_bytes.sort_by_key(|p| encode(p).unwrap());
        let mut by_tree = nodes;
        by_tree.sort();
        assert_eq!(by_bytes, by_tree);
    }

    #[test]
    fn methods() {
        let p = |s: &str| parse(s).unwrap();
        assert_eq!(level(&p("/")), 0);
        assert_eq!(level(&p("/1/2/3/")), 3);
        assert_eq!(ancestor(&p("/1/2/3/"), 1), Some(p("/1/2/")));
        assert_eq!(ancestor(&p("/1/2/3/"), 3), Some(p("/")));
        assert_eq!(ancestor(&p("/1/"), 2), None);
        assert!(is_descendant_of(&p("/1/2/"), &p("/1/")) && is_descendant_of(&p("/1/"), &p("/1/")) && !is_descendant_of(&p("/1/"), &p("/1/2/")) && is_descendant_of(&p("/3/"), &p("/")));
        assert_eq!(reparent(&p("/1/2/3/"), &p("/1/"), &p("/5/6/")), Some(p("/5/6/2/3/")));
        assert_eq!(reparent(&p("/2/"), &p("/1/"), &p("/5/")), None);
        assert_eq!(descendant(&p("/1/"), None, None).unwrap(), p("/1/1/"));
        assert_eq!(descendant(&p("/1/"), Some(&p("/1/4/")), None).unwrap(), p("/1/5/"));
        assert_eq!(descendant(&p("/1/"), None, Some(&p("/1/4/"))).unwrap(), p("/1/3/"));
        assert_eq!(descendant(&p("/1/"), Some(&p("/1/2/")), Some(&p("/1/9/"))).unwrap(), p("/1/3/"));
        assert!(descendant(&p("/1/"), Some(&p("/1/2/")), Some(&p("/1/3/"))).is_err(), "no whole label between 2 and 3");
        assert!(descendant(&p("/1/"), Some(&p("/2/1/")), None).is_err(), "not a child");
        assert!(descendant(&p("/1/"), None, Some(&p("/1/0/"))).is_err());
        assert!(descendant(&p("/1/"), Some(&p("/1/271/")), None).is_err(), "out of range");
    }

    #[test]
    fn what_cannot_be_stored_or_read_is_an_error() {
        assert!(parse("1/2/").is_err() && parse("/1").is_err() && parse("/a/").is_err() && parse("//").is_err() && parse("/-1/").is_err() && parse("/1.1/").is_err() && parse("/272/").is_err());
        assert!(encode(&[272]).is_err());
        // a fractional label (a 0 where the final 1 should be) and a prefix that is not in the table
        assert!(decode(&[0b0101_0000, 0b1000_0000]).is_err());
        assert!(decode(&[0b0010_0000]).is_err());
        assert_eq!(decode(&[]).unwrap(), Vec::<u64>::new());
    }
}
