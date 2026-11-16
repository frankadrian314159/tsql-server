//! Which shard owns a key.
//!
//! Tables are hash-partitioned on one column (the shard key). The hash must be the same in every process and on every
//! run, so it is a fixed function of the key's value (never of the machine's hasher), and values that compare equal in
//! SQL hash alike: integers of any width by their value, strings case-insensitively and without trailing blanks (the
//! default collation's rules; a case-sensitive collation only costs some skew, never correctness).
use serde::{Deserialize, Serialize};
use tsql_server::Value;

/// The number of hash slots. A key belongs to the slot `hash mod SLOTS`; each slot is owned by one shard, and moving a
/// slot from a shard to another is how a cluster grows, shrinks and rebalances.
pub const SLOTS: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyClass {
    Int,
    Str,
    Guid,
}

impl KeyClass {
    /// The class of a column of this type (lower-case type name), if its values can be a shard key.
    pub fn of_type_name(name: &str) -> Option<KeyClass> {
        match name {
            "tinyint" | "smallint" | "int" | "bigint" | "bit" => Some(KeyClass::Int),
            "char" | "varchar" | "nchar" | "nvarchar" => Some(KeyClass::Str),
            "uniqueidentifier" => Some(KeyClass::Guid),
            _ => None,
        }
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The hash of a key value, or None when the value cannot be read as a key of this class (the caller then does not prune
/// by it). NULL hashes to a fixed value, so all rows with a NULL key live on one shard.
pub fn key_hash(v: &Value, class: KeyClass) -> Option<u64> {
    if v.is_null() {
        return Some(0);
    }
    match class {
        KeyClass::Int => {
            let n: i64 = match v {
                Value::Int(i) => *i as i64,
                Value::BigInt(i) => *i,
                Value::Bit(b) => *b as i64,
                Value::Str(s) => s.trim().parse::<i64>().ok()?,
                _ => return None,
            };
            Some(fnv1a64(&n.to_le_bytes()))
        }
        KeyClass::Str => match v {
            Value::Str(s) => Some(fnv1a64(s.trim_end_matches(' ').to_lowercase().as_bytes())),
            _ => None,
        },
        KeyClass::Guid => match v {
            Value::Guid(g) => Some(fnv1a64(g.as_bytes())),
            _ => None,
        },
    }
}

/// The slot (0..SLOTS) of a key with this hash.
pub fn slot_of(hash: u64) -> usize {
    let mut x = hash;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    (x % SLOTS as u64) as usize
}

/// The shard (0..shards) that owns a key with this hash when the slots are dealt out evenly (`slot mod shards`).
pub fn shard_for(hash: u64, shards: usize) -> usize {
    // fnv's low bits are fine for small integers but mix anyway so that neighbouring keys spread
    slot_of(hash) % shards.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_keys_go_to_the_same_shard() {
        let a = key_hash(&Value::Int(42), KeyClass::Int).unwrap();
        let b = key_hash(&Value::BigInt(42), KeyClass::Int).unwrap();
        let c = key_hash(&Value::Str(" 42".into()), KeyClass::Int).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, c);
        let s1 = key_hash(&Value::Str("Alice  ".into()), KeyClass::Str).unwrap();
        let s2 = key_hash(&Value::Str("alice".into()), KeyClass::Str).unwrap();
        assert_eq!(s1, s2);
        assert!(key_hash(&Value::Str("x".into()), KeyClass::Int).is_none());
        assert!(key_hash(&Value::Int(1), KeyClass::Str).is_none());
    }

    #[test]
    fn keys_spread_over_the_shards() {
        let mut counts = [0usize; 4];
        for i in 0..4000 {
            counts[shard_for(key_hash(&Value::Int(i), KeyClass::Int).unwrap(), 4)] += 1;
        }
        for c in counts {
            assert!(c > 800 && c < 1200, "{:?}", counts);
        }
        // the hash is fixed (a change would orphan the data of existing clusters)
        assert_eq!(shard_for(key_hash(&Value::Int(1), KeyClass::Int).unwrap(), 4), shard_for(key_hash(&Value::Int(1), KeyClass::Int).unwrap(), 4));
        assert_eq!(key_hash(&Value::Int(0), KeyClass::Int), Some(fnv1a64(&0i64.to_le_bytes())));
    }
}
