//! How rows, columns and the root's entries become keys (25-kv-ranges.md
//! §28.2, §28.3).
//!
//! A user key is `r`, the row encoded, then the column. A row is encoded
//! as FoundationDB's tuple layer encodes byte strings, with a two-byte
//! end: each 0x00 becomes 0x00 0xFF, and the row ends with 0x00 0x01. The
//! encoding keeps the rows' order and no encoded row is a prefix of
//! another, so a key's row is found by decoding it, and every key of a
//! row lies in `[row_start(row), row_end(row))`.

/// The first byte of every user key.
pub const USER: u8 = b'r';

/// The first byte of a range's counts (§28.9): each key a delta, `c` +
/// a random id, summed to the counts of the rows the range holds. Before
/// every user key, so a split leaves them in the lower range.
pub const COUNTS: u8 = b'c';

/// A count delta's key.
pub fn count_key(id: u128) -> Vec<u8> {
    let mut out = vec![COUNTS];
    out.extend_from_slice(&id.to_be_bytes());
    out
}

/// The column that holds a row's own counts (§28.9). Columns that begin
/// with 0xFF are the store's.
pub const TALLY: &[u8] = &[0xFF];

/// The first byte of the deletion queue's keys (§28.6): before every
/// user key, so a split, which moves user keys only, leaves the queue in
/// the lower range.
pub const QUEUE: u8 = b'q';

/// A queue entry's key: when it is due (Unix seconds, big-endian), so
/// entries sort by it, then a random id.
pub fn queue_key(due: u64, id: u128) -> Vec<u8> {
    let mut out = vec![QUEUE];
    out.extend_from_slice(&due.to_be_bytes());
    out.extend_from_slice(&id.to_be_bytes());
    out
}

/// The end of the queue entries due by `now` (Unix seconds).
pub fn queue_due_end(now: u64) -> Vec<u8> {
    let mut out = vec![QUEUE];
    out.extend_from_slice(&(now.saturating_add(1)).to_be_bytes());
    out
}

/// The time now, in Unix seconds.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `b` with each 0x00 byte doubled to 0x00 0xFF, appended to `out`.
pub fn escape(out: &mut Vec<u8>, b: &[u8]) {
    for &c in b {
        out.push(c);
        if c == 0 {
            out.push(0xFF);
        }
    }
}

/// `row`, escaped and ended.
pub fn encode_row(row: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(row.len() + 2);
    escape(&mut out, row);
    out.extend_from_slice(&[0x00, 0x01]);
    out
}

/// The first key a row's columns can have: `r` and the encoded row.
pub fn row_start(row: &[u8]) -> Vec<u8> {
    let mut out = vec![USER];
    out.extend_from_slice(&encode_row(row));
    out
}

/// The first key past every column of `row`.
pub fn row_end(row: &[u8]) -> Vec<u8> {
    let mut out = row_start(row);
    // The end marker's 0x01 becomes 0x02: no encoded row has 0x00 0x02.
    *out.last_mut().unwrap() = 0x02;
    out
}

/// The key of `column` in `row`.
pub fn user_key(row: &[u8], column: &[u8]) -> Vec<u8> {
    let mut out = row_start(row);
    out.extend_from_slice(column);
    out
}

/// The first key of the rows that begin with `prefix`.
pub fn prefix_start(prefix: &[u8]) -> Vec<u8> {
    let mut out = vec![USER];
    escape(&mut out, prefix);
    out
}

/// The first key past every key that begins with `key`; none if `key` is
/// all 0xFF.
pub fn prefix_end(key: &[u8]) -> Option<Vec<u8>> {
    let last = key.iter().rposition(|&b| b != 0xFF)?;
    let mut out = key[..=last].to_vec();
    out[last] += 1;
    Some(out)
}

/// A user key's row and column; none if it isn't one.
pub fn split_key(key: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    let rest = key.strip_prefix(&[USER])?;
    let mut row = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        match (rest[i], rest.get(i + 1)) {
            (0x00, Some(0xFF)) => row.push(0),
            (0x00, Some(0x01)) => return Some((row, &rest[i + 2..])),
            (0x00, _) => return None,
            (c, _) => {
                row.push(c);
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    None
}

/// The root's record of the table called `name`.
pub fn table_key(name: &[u8]) -> Vec<u8> {
    let mut out = vec![b't'];
    out.extend_from_slice(name);
    out
}

/// The root's entry from table `id` to its name.
pub fn id_key(id: u64) -> Vec<u8> {
    let mut out = vec![b'i'];
    out.extend_from_slice(&id.to_be_bytes());
    out
}

/// The root's entries for table `id`'s ranges begin with this.
pub fn map_prefix(id: u64) -> Vec<u8> {
    let mut out = vec![b'm'];
    out.extend_from_slice(&id.to_be_bytes());
    out
}

/// The root's entry for table `id`'s range that starts at `start`.
pub fn map_key(id: u64, start: &[u8]) -> Vec<u8> {
    let mut out = map_prefix(id);
    out.extend_from_slice(&encode_row(start));
    out
}

/// The start row of a map entry's key, under `map_prefix(id)`.
pub fn map_start(id: u64, key: &[u8]) -> Option<Vec<u8>> {
    let enc = key.strip_prefix(map_prefix(id).as_slice())?;
    let mut k = vec![USER];
    k.extend_from_slice(enc);
    let (row, column) = split_key(&k)?;
    column.is_empty().then_some(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::SmallRng;
    use rand::{RngExt, SeedableRng};

    fn bytes(rng: &mut SmallRng) -> Vec<u8> {
        let n = rng.random_range(0..6);
        // Mostly 0x00, 0x01, 0x02 and 0xFF, where the encoding has cases.
        (0..n)
            .map(|_| match rng.random_range(0..6) {
                0 => 0x00,
                1 => 0x01,
                2 => 0x02,
                3 => 0xFF,
                _ => rng.random(),
            })
            .collect()
    }

    #[test]
    fn rows_keep_their_order_and_decode() {
        let mut rng = SmallRng::seed_from_u64(7);
        for _ in 0..20_000 {
            let (a, b) = (bytes(&mut rng), bytes(&mut rng));
            let (ca, cb) = (bytes(&mut rng), bytes(&mut rng));
            let (ka, kb) = (user_key(&a, &ca), user_key(&b, &cb));
            assert_eq!(a.cmp(&b), row_start(&a).cmp(&row_start(&b)), "{a:?} {b:?}");
            // Keys sort by row, then by column.
            assert_eq!(
                (&a, &ca).cmp(&(&b, &cb)),
                ka.cmp(&kb),
                "{a:?}/{ca:?} {b:?}/{cb:?}"
            );
            let (row, column) = split_key(&ka).unwrap();
            assert_eq!((row.as_slice(), column), (a.as_slice(), ca.as_slice()));
            // Every key of a row is within its bounds, and no other row's.
            assert!(row_start(&a) <= ka && ka < row_end(&a));
            if a != b {
                assert!(!(row_start(&a) <= kb && kb < row_end(&a)), "{a:?} {b:?}");
            }
            // A prefix's keys are exactly the rows that begin with it.
            let p = bytes(&mut rng);
            let within =
                prefix_start(&p) <= ka && prefix_end(&prefix_start(&p)).is_none_or(|e| ka < e);
            assert_eq!(within, a.starts_with(&p), "{p:?} {a:?}");
        }
    }

    #[test]
    fn map_entries_name_their_start() {
        for start in [&b""[..], b"a", b"\0", b"a\0b"] {
            assert_eq!(map_start(9, &map_key(9, start)).unwrap(), start);
            assert_eq!(map_start(8, &map_key(9, start)), None);
        }
        assert_eq!(prefix_end(b"a\xff\xff"), Some(b"b".to_vec()));
        assert_eq!(prefix_end(b"\xff"), None);
        assert_eq!(split_key(b"r\0"), None);
        assert_eq!(split_key(b"q\0\x01"), None);
    }
}
