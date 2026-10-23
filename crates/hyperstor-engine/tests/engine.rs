//! Behaviour of the engine's operations (spec/engine.md §7).

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

/// Digests (replication.md §8.5) depend on what an object reads as, not
/// on how it got there: written whole or in pieces, holes or zero blocks,
/// before or after a reopen. Any change to a block, an attribute, a key
/// or the size changes the digest.
#[test]
fn digests_follow_content_not_history() {
    let (d, mut sh) = fresh(4096, None);
    let mut data = block(1, 4);
    data[2 * BLOCK..3 * BLOCK].fill(0);
    // A: written whole, with a block of zeros in the middle.
    one(
        &mut sh,
        key("a"),
        vec![Op::Write {
            off: 0,
            data: data.clone(),
        }],
    )
    .unwrap();
    // B: the same in pieces, leaving that block a hole.
    let b = key("b");
    one(
        &mut sh,
        b.clone(),
        vec![Op::Write {
            off: 3 * BLOCK as u64,
            data: block(1, 1),
        }],
    )
    .unwrap();
    one(
        &mut sh,
        b.clone(),
        vec![Op::Write {
            off: 0,
            data: block(1, 2),
        }],
    )
    .unwrap();
    let digest = |sh: &Shard, k: &str| sh.digest(&key(k)).unwrap();
    assert_eq!(digest(&sh, "a"), digest(&sh, "b"));
    assert_eq!(sh.digest(&key("none")), None);

    let base = digest(&sh, "a");
    let changes: Vec<(&str, Vec<Op>)> = vec![
        (
            "block",
            vec![Op::Write {
                off: BLOCK as u64,
                data: block(2, 1),
            }],
        ),
        (
            "filled hole",
            vec![Op::Write {
                off: 2 * BLOCK as u64,
                data: block(3, 1),
            }],
        ),
        ("attribute", vec![Op::SetAttr(b"k".to_vec(), b"v".to_vec())]),
        ("key", vec![Op::KvPut(b"k".to_vec(), b"v".to_vec())]),
        ("size", vec![Op::Truncate(6 * BLOCK as u64)]),
    ];
    for (i, (what, ops)) in changes.into_iter().enumerate() {
        let k = format!("c{i}");
        one(
            &mut sh,
            key(&k),
            vec![Op::Write {
                off: 0,
                data: data.clone(),
            }],
        )
        .unwrap();
        assert_eq!(digest(&sh, &k), base, "{what}: same content first");
        one(&mut sh, key(&k), ops).unwrap();
        assert_ne!(digest(&sh, &k), base, "{what} must change the digest");
    }
    let before: Vec<_> = ["a", "b", "c0", "c3"].map(|k| digest(&sh, k)).into();
    drop(sh);
    let sh = reopen(&d);
    let after: Vec<_> = ["a", "b", "c0", "c3"].map(|k| digest(&sh, k)).into();
    assert_eq!(before, after, "digests survive a reopen");
}

/// Snapshot versions (engine.md §7.2, D104): a read through layers takes
/// each block from the first version that holds it; absorbing an older
/// version moves the blocks the newer one lacks without copying, frees
/// the ones it overrides, and survives a reopen.
#[test]
fn versions_read_through_layers_and_merge_without_copying() {
    let (d, mut sh) = fresh(4096, None);
    let v = |e: u64| ObjKey::new(1, 3, Namespace::Vol, b"chunk".to_vec(), e);
    // Epoch 1 holds blocks 0..4; epoch 3 overrides 1..3 and adds 6.
    one(
        &mut sh,
        v(1),
        vec![Op::Write {
            off: 0,
            data: block(1, 4),
        }],
    )
    .unwrap();
    one(
        &mut sh,
        v(3),
        vec![
            Op::Write {
                off: BLOCK as u64,
                data: block(3, 2),
            },
            Op::Write {
                off: 6 * BLOCK as u64,
                data: block(3, 1),
            },
        ],
    )
    .unwrap();
    // Another name, same prefix: not a version of this chunk.
    one(
        &mut sh,
        ObjKey::new(1, 3, Namespace::Vol, b"chunkX".to_vec(), 2),
        vec![Op::Touch],
    )
    .unwrap();
    assert_eq!(sh.versions(&v(0)), vec![1, 3]);
    let fills = |r: &[u8]| -> Vec<u8> { r.chunks(BLOCK).map(|b| b[0]).collect() };
    let r = sh.read_layered(&[v(3), v(1)], 0, 8 * BLOCK as u64).unwrap();
    assert_eq!(fills(&r), [1, 3, 3, 1, 0, 0, 3, 0]);
    // At epoch 2 only version 1 is visible.
    let r = sh.read_layered(&[v(1)], 0, 8 * BLOCK as u64).unwrap();
    assert_eq!(fills(&r), [1, 1, 1, 1, 0, 0, 0, 0]);
    assert!(matches!(
        sh.read_layered(&[v(9)], 0, 4096),
        Err(EngineError::NotFound)
    ));
    // Deleting the snapshot at epoch 1: version 3 absorbs version 1.
    let free = sh.stats().free_blocks;
    assert_eq!(
        one(&mut sh, v(3), vec![Op::Absorb(v(3))]),
        Err(OpError::SelfAbsorb)
    );
    one(&mut sh, v(3), vec![Op::Absorb(v(1))]).unwrap();
    assert!(sh.stat(&v(1)).is_none());
    assert_eq!(sh.versions(&v(0)), vec![3]);
    let r = sh.read(&v(3), 0, 8 * BLOCK as u64).unwrap();
    assert_eq!(
        fills(&r),
        [1, 3, 3, 1, 0, 0, 3, 0],
        "nothing seen at the head changed"
    );
    // The two overridden blocks were freed; the moved two were not copied.
    assert_eq!(sh.stats().free_blocks, free + 2);
    let r = sh.read(&v(3), 0, 8 * BLOCK as u64).unwrap();
    drop(sh);
    let sh = reopen(&d);
    assert_eq!(sh.read(&v(3), 0, 8 * BLOCK as u64).unwrap()[..], r[..]);
    assert_eq!(sh.stats().free_blocks, free + 2);
    assert!(sh.stat(&v(1)).is_none());
}

/// Paged checkpoints (D105): a checkpoint writes the pages changed since
/// the last one began; pages are cut as they fill and dropped once empty,
/// and a reopen loads every page.
#[test]
fn checkpoints_write_only_the_pages_that_changed() {
    let (d, mut sh) = fresh(8192, None);
    sh.set_checkpoint_page(1024);
    let put = |sh: &mut Shard, i: u64, v: u8| {
        one(
            sh,
            key(&format!("o{i:03}")),
            vec![Op::KvPut(b"v".to_vec(), vec![v; 100])],
        )
        .unwrap();
    };
    for i in 0..200 {
        put(&mut sh, i, 1);
    }
    sh.checkpoint().unwrap();
    let st = sh.stats();
    assert!(st.checkpoint_pages > 15, "pages are cut: {st:?}");
    // A bound, length, checksum and extent per page.
    assert!(
        st.checkpoint_directory_bytes > st.checkpoint_pages * 30,
        "{st:?}"
    );
    let written = |sh: &Shard| sh.stats().checkpoint_pages_written;
    // The checkpoint above began before the puts that followed its first
    // pages; one more settles everything.
    sh.checkpoint().unwrap();
    let w = written(&sh);
    sh.checkpoint().unwrap();
    assert_eq!(written(&sh) - w, 0, "nothing changed, no page written");
    put(&mut sh, 50, 2);
    let w = written(&sh);
    sh.checkpoint().unwrap();
    assert_eq!(written(&sh) - w, 1, "one change, one page");
    // Deleting a run of objects empties pages, which are dropped.
    let pages = sh.stats().checkpoint_pages;
    for i in 100..160 {
        one(&mut sh, key(&format!("o{i:03}")), vec![Op::Delete]).unwrap();
    }
    sh.checkpoint().unwrap();
    sh.checkpoint().unwrap();
    let st = sh.stats();
    assert!(st.checkpoint_pages < pages, "empty pages dropped: {st:?}");
    let idx = sh.index().clone();
    let bounds = idx.bounds.clone();
    drop(sh);
    let sh = reopen(&d);
    assert_eq!(sh.index(), &idx, "reopen reproduces the index exactly");
    assert_eq!(sh.index().bounds, bounds);
    assert_eq!(sh.stats().checkpoint_pages, st.checkpoint_pages);
    assert_eq!(sh.kv_get(&key("o050"), b"v"), Some(&[2u8; 100][..]));
    assert_eq!(sh.kv_get(&key("o120"), b"v"), None);
}
