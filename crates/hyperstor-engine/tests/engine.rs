//! Behaviour of the engine's operations (spec/design.md §7).

use hyperstor_engine::*;
use std::sync::Arc;

fn dev(blocks: u64) -> Arc<dyn BlockDev> {
    Arc::new(MemDev::new(blocks * BLOCK as u64))
}

fn cfg(shards: u32, log: Option<u64>) -> DeviceConfig {
    DeviceConfig {
        device_id: 7,
        cluster_id: 9,
        shards,
        log_blocks: log,
    }
}

fn key(name: &str) -> ObjKey {
    ObjKey::new(1, 3, Namespace::Vol, name.as_bytes().to_vec(), 0)
}

fn block(fill: u8, n: usize) -> Vec<u8> {
    vec![fill; n * BLOCK]
}

fn one(sh: &mut Shard, key: ObjKey, ops: Vec<Op>) -> Result<Committed, OpError> {
    sh.commit(vec![OpList { key, ops }]).unwrap().pop().unwrap()
}

fn fresh(blocks: u64, log: Option<u64>) -> (Arc<dyn BlockDev>, Shard) {
    let d = dev(blocks);
    format(&d, &cfg(1, log)).unwrap();
    let (_, mut shards) = open(&d).unwrap();
    (d, shards.pop().unwrap())
}

fn reopen(d: &Arc<dyn BlockDev>) -> Shard {
    open(d).unwrap().1.pop().unwrap()
}

#[test]
fn write_read_stat_and_holes() {
    let (_d, mut sh) = fresh(4096, None);
    let k = key("a");
    let c = one(
        &mut sh,
        k.clone(),
        vec![Op::Write {
            off: 4 * BLOCK as u64,
            data: block(1, 2),
        }],
    )
    .unwrap();
    assert!(c.revision > 0);
    let st = sh.stat(&k).unwrap();
    assert_eq!(st.size, 6 * BLOCK as u64);
    assert_eq!(st.blocks, 2);
    assert_eq!(st.revision, c.revision);
    let r = sh.read(&k, 0, 8 * BLOCK as u64).unwrap();
    assert!(r[..4 * BLOCK].iter().all(|&b| b == 0), "hole before");
    assert!(r[4 * BLOCK..6 * BLOCK].iter().all(|&b| b == 1));
    assert!(r[6 * BLOCK..].iter().all(|&b| b == 0), "hole after");
    assert!(matches!(
        sh.read(&key("none"), 0, BLOCK as u64),
        Err(EngineError::NotFound)
    ));
    assert!(matches!(
        sh.read(&k, 1, BLOCK as u64),
        Err(EngineError::Unaligned)
    ));
}

#[test]
fn small_overwrites_are_deferred_and_survive_reopen() {
    let (d, mut sh) = fresh(4096, None);
    let k = key("a");
    one(
        &mut sh,
        k.clone(),
        vec![Op::Write {
            off: 0,
            data: block(1, 32),
        }],
    )
    .unwrap();
    let free_before = sh.stats().free_blocks;
    one(
        &mut sh,
        k.clone(),
        vec![Op::Write {
            off: 8 * BLOCK as u64,
            data: block(2, 2),
        }],
    )
    .unwrap();
    assert_eq!(
        sh.stats().pending_blocks,
        2,
        "small overwrite goes through the log"
    );
    assert_eq!(sh.stats().free_blocks, free_before, "and allocates nothing");
    let r = sh.read(&k, 8 * BLOCK as u64, 2 * BLOCK as u64).unwrap();
    assert!(r.iter().all(|&b| b == 2), "reads see pending data");
    drop(sh);
    let sh = reopen(&d);
    let r = sh.read(&k, 0, 32 * BLOCK as u64).unwrap();
    for (i, b) in r.chunks(BLOCK).enumerate() {
        let want = if (8..10).contains(&i) { 2 } else { 1 };
        assert!(b.iter().all(|&x| x == want), "block {i}");
    }
}

#[test]
fn large_overwrites_redirect_and_free_old_blocks() {
    let (_d, mut sh) = fresh(4096, None);
    let k = key("a");
    one(
        &mut sh,
        k.clone(),
        vec![Op::Write {
            off: 0,
            data: block(1, 32),
        }],
    )
    .unwrap();
    let free = sh.stats().free_blocks;
    one(
        &mut sh,
        k.clone(),
        vec![Op::Write {
            off: 0,
            data: block(3, 32),
        }],
    )
    .unwrap();
    assert_eq!(sh.stats().pending_blocks, 0);
    assert_eq!(
        sh.stats().free_blocks,
        free,
        "old blocks freed after the flush"
    );
    assert!(
        sh.read(&k, 0, 32 * BLOCK as u64)
            .unwrap()
            .iter()
            .all(|&b| b == 3)
    );
}

#[test]
fn assertions_and_revisions() {
    let (_d, mut sh) = fresh(4096, None);
    let k = key("a");
    assert_eq!(
        one(&mut sh, k.clone(), vec![Op::AssertExists, Op::Touch]),
        Err(OpError::AssertFailed { current: 0 })
    );
    assert!(sh.stat(&k).is_none(), "a refused list changes nothing");
    let r1 = one(&mut sh, k.clone(), vec![Op::AssertAbsent, Op::Touch])
        .unwrap()
        .revision;
    let r2 = one(
        &mut sh,
        k.clone(),
        vec![
            Op::AssertRevision(r1),
            Op::SetAttr(b"x".to_vec(), b"1".to_vec()),
        ],
    )
    .unwrap()
    .revision;
    assert!(r2 > r1);
    assert_eq!(
        one(&mut sh, k.clone(), vec![Op::AssertRevision(r1), Op::Delete]),
        Err(OpError::AssertFailed { current: r2 })
    );
    // A pure assertion list changes nothing and reports the revision.
    assert_eq!(
        one(&mut sh, k.clone(), vec![Op::AssertRevision(r2)])
            .unwrap()
            .revision,
        r2
    );
    assert_eq!(
        one(&mut sh, k.clone(), vec![Op::Delete]).unwrap().revision,
        0
    );
    assert!(sh.stat(&k).is_none());
    let r3 = one(&mut sh, k.clone(), vec![Op::Touch]).unwrap().revision;
    assert!(r3 > r2, "revisions never repeat, even after delete");
    // Delete then write in one list: the object is recreated.
    let r4 = one(
        &mut sh,
        k.clone(),
        vec![
            Op::Delete,
            Op::Write {
                off: 0,
                data: block(5, 1),
            },
        ],
    )
    .unwrap()
    .revision;
    assert!(r4 > r3);
    assert_eq!(sh.stat(&k).unwrap().size, BLOCK as u64);
}

#[test]
fn unaligned_and_refused_lists_leave_no_trace() {
    let (_d, mut sh) = fresh(4096, None);
    let k = key("a");
    let res = sh
        .commit(vec![
            OpList {
                key: k.clone(),
                ops: vec![Op::Write {
                    off: 0,
                    data: block(1, 1),
                }],
            },
            OpList {
                key: k.clone(),
                ops: vec![Op::Write {
                    off: 3,
                    data: block(2, 1),
                }],
            },
            OpList {
                key: k.clone(),
                ops: vec![Op::Zero { off: 0, len: 100 }],
            },
            OpList {
                key: key("b"),
                ops: vec![Op::KvPut(b"k".to_vec(), b"v".to_vec())],
            },
        ])
        .unwrap();
    assert!(res[0].is_ok());
    assert_eq!(res[1], Err(OpError::Unaligned));
    assert_eq!(res[2], Err(OpError::Unaligned));
    assert!(res[3].is_ok());
    assert!(
        sh.read(&k, 0, BLOCK as u64)
            .unwrap()
            .iter()
            .all(|&b| b == 1)
    );
    assert_eq!(sh.kv_get(&key("b"), b"k"), Some(&b"v"[..]));
}

#[test]
fn zero_truncate_and_delete_free_space() {
    let (_d, mut sh) = fresh(4096, None);
    let k = key("a");
    let free0 = sh.stats().free_blocks;
    one(
        &mut sh,
        k.clone(),
        vec![Op::Write {
            off: 0,
            data: block(1, 16),
        }],
    )
    .unwrap();
    one(
        &mut sh,
        k.clone(),
        vec![Op::Zero {
            off: 0,
            len: 4 * BLOCK as u64,
        }],
    )
    .unwrap();
    assert_eq!(sh.stat(&k).unwrap().blocks, 12);
    let r = sh.read(&k, 0, 8 * BLOCK as u64).unwrap();
    assert!(r[..4 * BLOCK].iter().all(|&b| b == 0));
    assert!(r[4 * BLOCK..].iter().all(|&b| b == 1));
    one(
        &mut sh,
        k.clone(),
        vec![Op::Truncate(10 * BLOCK as u64 + 5)],
    )
    .unwrap();
    let st = sh.stat(&k).unwrap();
    assert_eq!(st.size, 10 * BLOCK as u64 + 5);
    assert_eq!(
        st.blocks, 7,
        "blocks 4..11 remain (the partial block stays)"
    );
    one(&mut sh, k.clone(), vec![Op::Delete]).unwrap();
    assert_eq!(sh.stats().free_blocks, free0, "everything returned");
}

#[test]
fn kv_tables_and_attrs() {
    let (d, mut sh) = fresh(4096, None);
    let k = ObjKey::new(2, 0, Namespace::Kv, b"range-1".to_vec(), 0);
    let mut ops = Vec::new();
    for i in 0..100u32 {
        ops.push(Op::KvPut(
            format!("key{i:03}").into_bytes(),
            i.to_le_bytes().to_vec(),
        ));
    }
    ops.push(Op::SetAttr(b"owner".to_vec(), b"s3".to_vec()));
    one(&mut sh, k.clone(), ops).unwrap();
    one(
        &mut sh,
        k.clone(),
        vec![
            Op::KvDelRange(b"key010".to_vec(), b"key020".to_vec()),
            Op::KvDel(b"key000".to_vec()),
        ],
    )
    .unwrap();
    let scan = sh.kv_scan(&k, b"key005", Some(b"key025"), 100);
    let keys: Vec<String> = scan
        .iter()
        .map(|(k, _)| String::from_utf8(k.clone()).unwrap())
        .collect();
    assert_eq!(keys.len(), 5 + 5);
    assert_eq!(keys[0], "key005");
    assert_eq!(keys[5], "key020");
    assert_eq!(sh.kv_scan(&k, b"", None, 3).len(), 3);
    drop(sh);
    let sh = reopen(&d);
    assert_eq!(sh.kv_get(&k, b"key050"), Some(&50u32.to_le_bytes()[..]));
    assert_eq!(sh.kv_get(&k, b"key000"), None);
    assert_eq!(
        sh.attrs(&k).unwrap().get(&b"owner"[..]),
        Some(&b"s3".to_vec())
    );
}

#[test]
fn log_wraps_and_checkpoints_keep_state() {
    // A tiny log forces many checkpoints and ring wraps.
    let (d, mut sh) = fresh(4096, Some(64));
    let mut expect = std::collections::BTreeMap::new();
    for i in 0..400u64 {
        let k = key(&format!("o{}", i % 13));
        let fill = (i % 251) as u8;
        let blocks = 1 + (i % 4) as usize;
        let off = (i % 5) * BLOCK as u64;
        one(
            &mut sh,
            k.clone(),
            vec![Op::Write {
                off,
                data: block(fill, blocks),
            }],
        )
        .unwrap();
        for b in 0..blocks as u64 {
            expect.insert((k.clone(), off / BLOCK as u64 + b), fill);
        }
    }
    let st = sh.stats();
    assert!(st.checkpoints > 5, "{st:?}");
    let check = |sh: &Shard| {
        for ((k, lb), fill) in &expect {
            let r = sh.read(k, lb * BLOCK as u64, BLOCK as u64).unwrap();
            assert!(r.iter().all(|b| b == fill), "{k:?} block {lb}");
        }
    };
    check(&sh);
    let idx = sh.index().clone();
    drop(sh);
    let sh = reopen(&d);
    assert_eq!(sh.index(), &idx, "reopen reproduces the index exactly");
    check(&sh);
}

#[test]
fn no_space_and_too_large() {
    let (_d, mut sh) = fresh(512, Some(64));
    let free = sh.stats().free_blocks;
    let k = key("big");
    assert_eq!(
        one(
            &mut sh,
            k.clone(),
            vec![Op::Write {
                off: 0,
                data: block(1, free as usize + 1)
            }]
        ),
        Err(OpError::NoSpace)
    );
    let huge: Vec<Op> = (0..2000)
        .map(|i| Op::KvPut(format!("{i:08}").into_bytes(), vec![0; 64]))
        .collect();
    assert_eq!(one(&mut sh, k.clone(), huge), Err(OpError::TooLarge));
    assert!(sh.stat(&k).is_none());
}

#[test]
fn shards_are_independent() {
    let d = dev(8192);
    let info = format(&d, &cfg(4, None)).unwrap();
    assert_eq!(info.regions.len(), 4);
    let (_, mut shards) = open(&d).unwrap();
    for (i, sh) in shards.iter_mut().enumerate() {
        one(
            sh,
            key("same-name"),
            vec![Op::Write {
                off: 0,
                data: block(i as u8 + 1, 1),
            }],
        )
        .unwrap();
    }
    drop(shards);
    let (_, shards) = open(&d).unwrap();
    for (i, sh) in shards.iter().enumerate() {
        assert!(
            sh.read(&key("same-name"), 0, BLOCK as u64)
                .unwrap()
                .iter()
                .all(|&b| b == i as u8 + 1)
        );
    }
}

#[test]
fn bad_devices_are_refused() {
    let d = dev(4096);
    assert!(matches!(open(&d), Err(EngineError::NotFormatted)));
    let small = dev(32);
    assert!(matches!(
        format(&small, &cfg(1, None)),
        Err(EngineError::TooSmall)
    ));
}

#[test]
fn corrupted_data_is_detected_on_read() {
    let (d, mut sh) = fresh(4096, None);
    let k = key("a");
    one(
        &mut sh,
        k.clone(),
        vec![Op::Write {
            off: 0,
            data: block(1, 16),
        }],
    )
    .unwrap();
    let pb = match sh.index().objects[&k].lookup(3) {
        hyperstor_engine::index::Loc::Mapped { pblock, .. } => pblock,
        _ => unreachable!(),
    };
    let mut junk = AlignedBuf::zeroed(BLOCK);
    junk.fill(0xee);
    d.write(pb * BLOCK as u64, &junk).unwrap();
    assert!(matches!(
        sh.read(&k, 0, 16 * BLOCK as u64),
        Err(EngineError::Checksum { lblock: 3 })
    ));
    assert!(sh.read(&k, 0, 3 * BLOCK as u64).is_ok());
}

#[test]
fn io_error_poisons_until_reopen() {
    let d: Arc<FaultDev> = Arc::new(FaultDev::new(4096 * BLOCK as u64));
    let dd: Arc<dyn BlockDev> = d.clone();
    format(&dd, &cfg(1, None)).unwrap();
    let mut sh = open(&dd).unwrap().1.pop().unwrap();
    one(
        &mut sh,
        key("a"),
        vec![Op::Write {
            off: 0,
            data: block(1, 1),
        }],
    )
    .unwrap();
    d.fail_after(0);
    assert!(
        sh.commit(vec![OpList {
            key: key("a"),
            ops: vec![Op::Touch]
        }])
        .is_err()
    );
    assert!(matches!(
        sh.commit(vec![OpList {
            key: key("a"),
            ops: vec![Op::Touch]
        }]),
        Err(EngineError::Poisoned)
    ));
}
