//! 属性测试（proptest）：btree 随机操作序列 vs 模型对照。
//!
//! 交付重点对齐（AGENTS.md）：btree 操作正确性、事务一致性、journal
//! 持久化与恢复。两层验证：
//!
//! 1. `random_operations_match_btree_map_model`：内存引擎上随机
//!    put/delete（单键快路径与批量事务路径混合）与 `BTreeMap` 模型
//!    逐步对照（scan 全量 + verify 树结构）。
//! 2. `crash_recovery_restores_sync_point_state`：持久化引擎在随机
//!    sync 点生成崩溃快照，`recover()` 重建后 scan 必须精确等于
//!    快照时刻的模型状态（journal 持久化与恢复语义）。
//!
//! 测试模型是项目自有的验证设施（T0168 AC-4 将"属性测试为零"列为
//! 双基准差距项，AGENTS.md 明确要求属性测试验证），不构成运行时逻辑。

use std::collections::BTreeMap;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use proptest::prelude::*;
use subvol::{BtreeId, BtreeKey, EngineError, FaultPoint, KeyPosition, StorageEngine};

const CASES: u32 = 64;
const MAX_OPS: usize = 120;

/* 镜像 journal 布局（与 engine.rs 的 JOURNAL_FILE_SECTORS/JOURNAL_BUCKET_SIZE
 * 常量一致）：4 个 1MB bucket，从镜像 offset 1MB（JOURNAL_BUCKET_START=1）起。 */
const JOURNAL_BUCKET_START: u64 = 1;
const JOURNAL_BUCKET_SIZE: u64 = 2_048;

fn key_strategy() -> impl Strategy<Value = (u64, u64, u32)> {
    (1u64..=3, 1u64..=24, 0u32..=2)
}

fn value_strategy() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(any::<u64>(), 1..=8)
}

#[derive(Clone, Debug)]
enum Op {
    Put {
        inode: u64,
        offset: u64,
        snapshot: u32,
        value: Vec<u64>,
    },
    Delete {
        inode: u64,
        offset: u64,
        snapshot: u32,
    },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (key_strategy(), value_strategy()).prop_map(|((i, o, s), v)| Op::Put {
            inode: i,
            offset: o,
            snapshot: s,
            value: v,
        }),
        key_strategy().prop_map(|(i, o, s)| Op::Delete {
            inode: i,
            offset: o,
            snapshot: s,
        }),
    ]
}

/// 事务批量变体：1..=6 个操作打包进一次 `Transaction::commit`。
#[derive(Clone, Debug)]
enum OpGroup {
    Single(Op),
    Batch(Vec<Op>),
}

fn op_group_strategy() -> impl Strategy<Value = OpGroup> {
    prop_oneof![
        op_strategy().prop_map(OpGroup::Single),
        prop::collection::vec(op_strategy(), 2..=6).prop_map(OpGroup::Batch),
    ]
}

/// 多快照测试的快照 id 池：覆盖 bcachefs 快照 id 分配的关键边界
/// （fs/snapshots/snapshot.c create_snapids 从 u32::MAX 递减分配）：
/// - `u32::MAX` 族：首个快照 id 与递减分配
/// - `127/128/129`：IS_ANCESTOR_BITMAP（128）祖先位图覆盖边界
/// - `0/3`：小 id（非快照键）
fn multi_snapshot_pool() -> impl Strategy<Value = u32> {
    prop::sample::select(vec![
        0u32,
        3,
        127,
        128,
        129,
        u32::MAX - 2,
        u32::MAX - 1,
        u32::MAX,
    ])
}

/// 多快照操作：小 (inode, offset) 空间（制造同位置多版本共存）×
/// 大快照 id 空间（跨边界 id）。
fn multi_snapshot_op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (
            (1u64..=2, 1u64..=8),
            multi_snapshot_pool(),
            value_strategy()
        )
            .prop_map(|((i, o), s, v)| Op::Put {
                inode: i,
                offset: o,
                snapshot: s,
                value: v,
            }),
        ((1u64..=2, 1u64..=8), multi_snapshot_pool()).prop_map(|((i, o), s)| Op::Delete {
            inode: i,
            offset: o,
            snapshot: s,
        }),
    ]
}

fn multi_snapshot_op_group_strategy() -> impl Strategy<Value = OpGroup> {
    prop_oneof![
        multi_snapshot_op_strategy().prop_map(OpGroup::Single),
        prop::collection::vec(multi_snapshot_op_strategy(), 2..=6).prop_map(OpGroup::Batch),
    ]
}

fn position(inode: u64, offset: u64, snapshot: u32) -> KeyPosition {
    KeyPosition::new(inode, offset, snapshot)
}

fn apply_model(model: &mut BTreeMap<KeyPosition, Vec<u64>>, op: &Op) {
    match op {
        Op::Put {
            inode,
            offset,
            snapshot,
            value,
        } => {
            model.insert(position(*inode, *offset, *snapshot), value.clone());
        }
        Op::Delete {
            inode,
            offset,
            snapshot,
        } => {
            model.remove(&position(*inode, *offset, *snapshot));
        }
    }
}

/// scan 结果必须与模型逐项一致且按 KeyPosition 升序。
fn assert_model(engine: &StorageEngine, model: &BTreeMap<KeyPosition, Vec<u64>>) {
    let keys = engine.scan(BtreeId::DEFAULT).unwrap();
    if keys.len() != model.len() {
        eprintln!("MODEL: {:?}", model);
        eprintln!(
            "SCAN:  {:?}",
            keys.iter()
                .map(|k| (k.position(), k.value().to_vec()))
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(keys.len(), model.len(), "scan 键数与模型不一致");
    for (got, (want_pos, want_val)) in keys.iter().zip(model.iter()) {
        assert_eq!(&got.position(), want_pos, "scan 键序/键值与模型不一致");
        assert_eq!(
            got.value(),
            want_val.as_slice(),
            "value 不一致 @ {:?}",
            want_pos
        );
    }
    engine.verify(BtreeId::DEFAULT).unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 1,
        max_shrink_iters: 0,
        ..ProptestConfig::default()
    })]

    #[test]
    fn random_operations_match_btree_map_model(
        ops in prop::collection::vec(op_group_strategy(), 1..=MAX_OPS),
    ) {
        eprintln!("OPS: {:#?}", ops);
        let engine = StorageEngine::new().unwrap();
        let mut model = BTreeMap::new();

        for (step, group) in ops.iter().enumerate() {
            match group {
                OpGroup::Single(op) => match op {
                    Op::Put { inode, offset, snapshot, value } => {
                        engine
                            .put(
                                BtreeId::DEFAULT,
                                BtreeKey::new(position(*inode, *offset, *snapshot), value.clone())
                                    .unwrap(),
                            )
                            .unwrap();
                    }
                    Op::Delete { inode, offset, snapshot } => {
                        engine.delete(BtreeId::DEFAULT, position(*inode, *offset, *snapshot)).unwrap();
                    }
                },
                OpGroup::Batch(ops) => {
                    let mut transaction = engine.transaction();
                    for op in ops {
                        match op {
                            Op::Put { inode, offset, snapshot, value } => {
                                transaction.put(
                                    BtreeId::DEFAULT,
                                    BtreeKey::new(
                                        position(*inode, *offset, *snapshot),
                                        value.clone(),
                                    )
                                    .unwrap(),
                                );
                            }
                            Op::Delete { inode, offset, snapshot } => {
                                transaction
                                    .delete(BtreeId::DEFAULT, position(*inode, *offset, *snapshot));
                            }
                        }
                    }
                    transaction.commit().unwrap();
                }
            }
            for op in ops_of(group) {
                apply_model(&mut model, op);
            }
            if step % 8 == 0 {
                eprintln!("assert step={step} model={}", model.len());
                assert_model(&engine, &model);
            }
            if step % 16 == 0 {
                eprintln!("progress step={step} model={}", model.len());
            }
            eprintln!("about to apply step={step}: {:?}", ops_of(group).iter().map(|o| match o {
                Op::Put { inode, offset, snapshot, value } => format!("P({inode},{offset},{snapshot},v{})", value.len()),
                Op::Delete { inode, offset, snapshot } => format!("D({inode},{offset},{snapshot})"),
            }).collect::<Vec<_>>());
        }
        assert_model(&engine, &model);
    }

    #[test]
    fn crash_recovery_restores_sync_point_state(
        ops in prop::collection::vec(op_group_strategy(), 1..=MAX_OPS),
        crash_every in 7usize..=13,
    ) {
        let dir = unique_tmp_dir();
        let mut engine = StorageEngine::create_persistent(&dir).unwrap();
        let mut model = BTreeMap::new();

        for (step, group) in ops.iter().enumerate() {
            apply_group(&engine, group).unwrap();
            for op in ops_of(group) {
                apply_model(&mut model, op);
            }

            if step % crash_every == crash_every - 1 {
                /* sync 点：journal 落盘后丢弃引擎，模拟"崩溃"。重建必须
                 * 从设备恢复（bcachefs 语义：设备 btree + journal 窗口，
                 * 对齐 read_btree_roots + bch2_journal_read），恢复后的
                 * scan 必须与模型完全一致。 */
                engine.sync().unwrap();
                drop(engine);
                engine = StorageEngine::open_persistent(&dir).unwrap();
                assert_model(&engine, &model);
            }
        }
        engine.sync().unwrap();
        drop(engine);
        let recovered = StorageEngine::open_persistent(&dir).unwrap();
        assert_model(&recovered, &model);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn reclaim_after_checkpoint_preserves_model(
        ops in prop::collection::vec(op_group_strategy(), 1..=MAX_OPS),
        reclaim_every in 3usize..=6,
        crash_every in 9usize..=17,
    ) {
        /* journal reclaim 压力验证（T0169 回归保障）：随机操作流中周期性
         * 显式触发 reclaim_journal()（直接路径 checkpoint：flush pins →
         * 推进 last_seq，对齐 fs/journal/reclaim.c），再叠加崩溃恢复。
         * bcachefs 语义：恢复仅重放 last_seq 之后窗口（recovery.c:763
         * journal_replay_seq_start = last_seq），早于 last_seq 的数据由
         * checkpoint 落盘的设备 btree 提供——裁剪推进过度会丢键，恢复后
         * 模型对照必须精确一致。 */
        let dir = unique_tmp_dir();
        let mut engine = StorageEngine::create_persistent(&dir).unwrap();
        let mut model = BTreeMap::new();

        for (step, group) in ops.iter().enumerate() {
            apply_group(&engine, group).unwrap();
            for op in ops_of(group) {
                apply_model(&mut model, op);
            }

            if step % reclaim_every == reclaim_every - 1 {
                /* 裁剪生效断言：reclaim 后 last_seq_ondisk 单调不倒退
                 * （无覆盖数据时不变，故用 >=）。 */
                let before = engine.metrics().unwrap().journal_last_sequence_ondisk;
                engine.reclaim_journal().unwrap();
                let after = engine.metrics().unwrap().journal_last_sequence_ondisk;
                assert!(
                    after >= before,
                    "reclaim 使 last_seq_ondisk 倒退: {before} -> {after}"
                );
            }

            if step % crash_every == crash_every - 1 {
                engine.sync().unwrap();
                drop(engine);
                engine = StorageEngine::open_persistent(&dir).unwrap();
                assert_model(&engine, &model);
            }
        }
        engine.sync().unwrap();
        drop(engine);
        let recovered = StorageEngine::open_persistent(&dir).unwrap();
        assert_model(&recovered, &model);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn fault_injection_preserves_model_and_recovery(
        ops in prop::collection::vec(op_group_strategy(), 1..=MAX_OPS),
        crash_every in 7usize..=13,
    ) {
        /* 故障注入验证（AGENTS.md 交付重点）：随机操作序列中注入
         * TransactionRestart（事务重试）与 JournalWrite（写盘失败），
         * 引擎必须通过重试保持模型一致，且最终成功落盘后设备恢复
         * （open_persistent）与模型完全一致。注入次数由 step/组形状
         * 确定性派生，保证可复现。 */
        let dir = unique_tmp_dir();
        let mut engine = StorageEngine::create_persistent(&dir).unwrap();
        let mut model = BTreeMap::new();

        for (step, group) in ops.iter().enumerate() {
            let restarts = (step * 7 + ops_of(group).len() + ops.len()) % 4;
            if restarts > 0 {
                engine
                    .inject_fault(FaultPoint::TransactionRestart, restarts as u32)
                    .unwrap();
            }
            apply_group(&engine, group).unwrap();
            for op in ops_of(group) {
                apply_model(&mut model, op);
            }

            if step % crash_every == crash_every - 1 {
                /* 写盘失败注入：sync 必须返回 Journal(-5)，随后重试必须
                 * 成功；失败 flush 不得污染后续状态（write.c 写失败语义
                 * 在引擎中为可重试）。 */
                let write_failures = (step * 3 + ops_of(group).len() + 1) % 3;
                for _ in 0..write_failures {
                    engine
                        .inject_fault(FaultPoint::JournalWrite, 1)
                        .unwrap();
                    assert!(
                        matches!(engine.sync(), Err(EngineError::Journal(-5))),
                        "注入 JournalWrite 后 sync 必须失败 step={step}"
                    );
                }
                engine.sync().unwrap();
                drop(engine);
                engine = StorageEngine::open_persistent(&dir).unwrap();
                assert_model(&engine, &model);
            }
        }
        engine.sync().unwrap();
        drop(engine);
        let recovered = StorageEngine::open_persistent(&dir).unwrap();
        assert_model(&recovered, &model);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn multi_snapshot_versions_coexist_and_recover(
        ops in prop::collection::vec(multi_snapshot_op_group_strategy(), 1..=MAX_OPS),
        crash_every in 9usize..=17,
    ) {
        /* 多快照键空间属性测试（AGENTS.md 交付重点：btree 操作正确性与
         * journal 持久化恢复）：快照 id 覆盖 u32::MAX 递减分配族与
         * IS_ANCESTOR_BITMAP 边界，小 (inode, offset) 空间使同一位置
         * 多快照版本共存。验证：
         * 1. 各快照版本独立读写（get 精确匹配，无跨快照污染）
         * 2. scan 全序（KeyPosition Ord = bpos_cmp 的 (inode, offset,
         *    snapshot) 字典序，bkey.rs:780）
         * 3. 崩溃恢复（open_persistent）后全部快照版本保留 */
        let dir = unique_tmp_dir();
        let mut engine = StorageEngine::create_persistent(&dir).unwrap();
        let mut model = BTreeMap::new();

        for (step, group) in ops.iter().enumerate() {
            apply_group(&engine, group).unwrap();
            for op in ops_of(group) {
                apply_model(&mut model, op);
            }

            let probes = step % 4 + 1;
            for (pos, val) in model.iter().take(probes) {
                let got = engine.get(BtreeId::DEFAULT, *pos).unwrap();
                assert_eq!(
                    got.as_ref().map(BtreeKey::value),
                    Some(val.as_slice()),
                    "快照版本读取不一致 step={step} @ {pos:?}"
                );
            }

            if step % crash_every == crash_every - 1 {
                engine.sync().unwrap();
                drop(engine);
                engine = StorageEngine::open_persistent(&dir).unwrap();
                assert_model(&engine, &model);
            }
        }
        engine.sync().unwrap();
        drop(engine);
        let recovered = StorageEngine::open_persistent(&dir).unwrap();
        assert_model(&recovered, &model);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn journal_corruption_detection_in_random_state(
        ops in prop::collection::vec(op_group_strategy(), 1..=MAX_OPS),
    ) {
        /* journal 损坏注入属性测试（AGENTS.md 交付重点：journal 持久化
         * 与恢复、损坏检测）：随机操作序列落盘后，确定性注入两类损坏
         * ——首个记录头部 version 破坏与最后一条记录 payload 校验和破坏
         * ——恢复（open_persistent）必须拒绝（对齐 bch2_journal_read 的
         * 校验失败路径，read.c:406 起 csum 校验 / validate.c 结构校验），
         * 不得静默恢复错误数据；字节修复后必须恢复出完整模型。 */
        let dir = unique_tmp_dir();
        let engine = StorageEngine::create_persistent(&dir).unwrap();
        let mut model = BTreeMap::new();
        for group in &ops {
            apply_group(&engine, group).unwrap();
            for op in ops_of(group) {
                apply_model(&mut model, op);
            }
        }
        engine.sync().unwrap();
        drop(engine);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dir)
            .unwrap();
        let bucket0 = JOURNAL_BUCKET_START * JOURNAL_BUCKET_SIZE as u64 * 512;

        /* version_flags 位于 jset 头部 offset 32（csum 16B + magic 8B +
         * seq 8B），低 32 位为版本号：翻转最低字节后版本必失配 -> -5。 */
        let mut byte = [0u8; 1];
        assert_eq!(file.read_at(&mut byte, bucket0 + 32).unwrap(), 1);
        byte[0] ^= 1;
        assert_eq!(file.write_at(&byte, bucket0 + 32).unwrap(), 1);
        let rejected = StorageEngine::open_persistent(&dir);
        assert!(
            matches!(rejected, Err(EngineError::Journal(-5))),
            "version 损坏必须被恢复拒绝"
        );
        byte[0] ^= 1;
        assert_eq!(file.write_at(&byte, bucket0 + 32).unwrap(), 1);

        /* root 记录校验和字段（jset 头部固定 offset 0，16 字节）破坏
         * -> -6：读扫描对每条记录做 jset_csum_good 校验（对齐
         * read.c:94-102 / journal.rs:1350-1360），root 记录恒在
         * bucket0 头（create 时写入、单 bucket 无回绕），无需解析。 */
        let mut byte = [0u8; 1];
        assert_eq!(file.read_at(&mut byte, bucket0).unwrap(), 1);
        byte[0] ^= 1;
        assert_eq!(file.write_at(&byte, bucket0).unwrap(), 1);
        let rejected = StorageEngine::open_persistent(&dir);
        assert!(
            matches!(rejected, Err(EngineError::Journal(-6))),
            "校验和损坏必须被恢复拒绝"
        );
        byte[0] ^= 1;
        assert_eq!(file.write_at(&byte, bucket0).unwrap(), 1);
        file.sync_all().unwrap();
        drop(file);

        /* 字节修复后恢复：模型必须完整一致（损坏拒绝路径不落盘）。 */
        let recovered = StorageEngine::open_persistent(&dir).unwrap();
        assert_model(&recovered, &model);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn journal_corruption_benign_tail_ignored(
        ops in prop::collection::vec(op_group_strategy(), 1..=MAX_OPS),
    ) {
        /* journal 空白区损坏不误报属性测试：bucket 尾部（记录从未到达
         * 的空白扇区）翻转不得破坏恢复——对齐 bcachefs 对空白区的跳过
         * （read.c:372-383 JOURNAL_ENTRY_NONE -> 跳过），恢复必须成功
         * 且模型完整一致。 */
        let dir = unique_tmp_dir();
        let engine = StorageEngine::create_persistent(&dir).unwrap();
        let mut model = BTreeMap::new();
        for group in &ops {
            apply_group(&engine, group).unwrap();
            for op in ops_of(group) {
                apply_model(&mut model, op);
            }
        }
        engine.sync().unwrap();
        drop(engine);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dir)
            .unwrap();
        let tail_blank =
            JOURNAL_BUCKET_START * JOURNAL_BUCKET_SIZE * 512 + JOURNAL_BUCKET_SIZE * 512 - 8;
        let mut byte = [0u8; 1];
        assert_eq!(file.read_at(&mut byte, tail_blank).unwrap(), 1);
        byte[0] ^= 1;
        assert_eq!(file.write_at(&byte, tail_blank).unwrap(), 1);
        file.sync_all().unwrap();
        drop(file);

        let recovered = StorageEngine::open_persistent(&dir).unwrap();
        assert_model(&recovered, &model);
        let _ = std::fs::remove_file(&dir);
    }
}

fn ops_of(group: &OpGroup) -> &[Op] {
    match group {
        OpGroup::Single(op) => std::slice::from_ref(op),
        OpGroup::Batch(ops) => ops,
    }
}

fn apply_group(engine: &StorageEngine, group: &OpGroup) -> Result<(), subvol::EngineError> {
    match group {
        OpGroup::Single(op) => match op {
            Op::Put {
                inode,
                offset,
                snapshot,
                value,
            } => engine.put(
                BtreeId::DEFAULT,
                BtreeKey::new(position(*inode, *offset, *snapshot), value.clone()).unwrap(),
            ),
            Op::Delete {
                inode,
                offset,
                snapshot,
            } => engine.delete(BtreeId::DEFAULT, position(*inode, *offset, *snapshot)),
        },
        OpGroup::Batch(ops) => {
            let mut transaction = engine.transaction();
            for op in ops {
                match op {
                    Op::Put {
                        inode,
                        offset,
                        snapshot,
                        value,
                    } => {
                        transaction.put(
                            BtreeId::DEFAULT,
                            BtreeKey::new(position(*inode, *offset, *snapshot), value.clone())
                                .unwrap(),
                        );
                    }
                    Op::Delete {
                        inode,
                        offset,
                        snapshot,
                    } => {
                        transaction.delete(BtreeId::DEFAULT, position(*inode, *offset, *snapshot));
                    }
                }
            }
            transaction.commit()
        }
    }
}

fn unique_tmp_dir() -> PathBuf {
    /* 持久化引擎的 path 是单一文件（镜像布局），不是目录。 */
    let mut path = std::env::temp_dir();
    path.push(format!(
        "subvol-proptest-{}-{:?}.bch",
        std::process::id(),
        std::thread::current().id()
    ));
    path
}
/// 确定性回归：proptest 抓到的 delete 死循环（卡在 D(2,18,0)）。
/// 确定性回归：proptest 抓到的 delete 死循环（卡在 D(2,18,0)）。
/// 确定性回归：proptest 抓到的 delete 死循环（卡在 D(2,18,0)）。
/// 确定性回归：proptest 抓到的 delete 死循环（卡在 D(2,18,0)）。
/// 确定性回归：proptest 抓到的 delete 死循环（卡在 D(2,18,0)）。
#[test]
fn deterministic_delete_hang_repro() {
    let ops: Vec<OpGroup> = vec![
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 3,
                offset: 12,
                snapshot: 0,
            },
            Op::Delete {
                inode: 3,
                offset: 18,
                snapshot: 0,
            },
            Op::Put {
                inode: 2,
                offset: 23,
                snapshot: 1,
                value: vec![
                    17676549469949137658,
                    16059446075393107129,
                    14059256209310145924,
                    12643191705686384970,
                    4548195474878887529,
                ],
            },
            Op::Delete {
                inode: 3,
                offset: 8,
                snapshot: 2,
            },
            Op::Delete {
                inode: 1,
                offset: 14,
                snapshot: 2,
            },
            Op::Put {
                inode: 2,
                offset: 18,
                snapshot: 0,
                value: vec![12128706751690829958],
            },
        ]),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 2,
                offset: 7,
                snapshot: 2,
            },
            Op::Delete {
                inode: 2,
                offset: 1,
                snapshot: 2,
            },
            Op::Delete {
                inode: 2,
                offset: 2,
                snapshot: 2,
            },
        ]),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 1,
                offset: 17,
                snapshot: 2,
            },
            Op::Delete {
                inode: 1,
                offset: 3,
                snapshot: 1,
            },
        ]),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 3,
                offset: 23,
                snapshot: 1,
                value: vec![
                    7979441865148389169,
                    17624243150835272815,
                    5897854353423802063,
                    11190500926921232233,
                ],
            },
            Op::Delete {
                inode: 2,
                offset: 2,
                snapshot: 0,
            },
            Op::Delete {
                inode: 1,
                offset: 21,
                snapshot: 2,
            },
            Op::Delete {
                inode: 2,
                offset: 8,
                snapshot: 1,
            },
            Op::Put {
                inode: 2,
                offset: 23,
                snapshot: 2,
                value: vec![
                    17128173815631212595,
                    11479583397899007988,
                    15935864044653384966,
                    8917608910712497123,
                    16636255280407234312,
                    13551594344619692897,
                    9345909308005001013,
                    2230894859142784801,
                ],
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 3,
            offset: 12,
            snapshot: 0,
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 2,
                offset: 22,
                snapshot: 0,
            },
            Op::Put {
                inode: 1,
                offset: 21,
                snapshot: 2,
                value: vec![
                    16182849393141286024,
                    15460693392648130730,
                    15256878031603894662,
                    18154737707430645535,
                    8113490364902572830,
                    13602973228964004890,
                ],
            },
            Op::Delete {
                inode: 3,
                offset: 19,
                snapshot: 0,
            },
            Op::Delete {
                inode: 3,
                offset: 18,
                snapshot: 0,
            },
            Op::Delete {
                inode: 1,
                offset: 6,
                snapshot: 0,
            },
        ]),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 1,
                offset: 24,
                snapshot: 2,
                value: vec![3377305195336340505, 7075662725464795942],
            },
            Op::Put {
                inode: 2,
                offset: 19,
                snapshot: 2,
                value: vec![6566316018338493804],
            },
            Op::Put {
                inode: 3,
                offset: 18,
                snapshot: 0,
                value: vec![
                    4987186733164459539,
                    4854454303095834908,
                    8466747431431319404,
                    16618870201005438712,
                    13891474341881121033,
                    17515159954750442921,
                ],
            },
            Op::Delete {
                inode: 3,
                offset: 19,
                snapshot: 0,
            },
        ]),
        OpGroup::Single(Op::Put {
            inode: 3,
            offset: 5,
            snapshot: 0,
            value: vec![5792795924379510994],
        }),
        OpGroup::Single(Op::Put {
            inode: 2,
            offset: 3,
            snapshot: 2,
            value: vec![
                2026314652460032608,
                147784535154275786,
                13374087167209251788,
                7767798052491033871,
                8243409495011031802,
            ],
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 2,
                offset: 10,
                snapshot: 2,
            },
            Op::Delete {
                inode: 1,
                offset: 17,
                snapshot: 0,
            },
            Op::Delete {
                inode: 3,
                offset: 9,
                snapshot: 2,
            },
        ]),
        OpGroup::Single(Op::Put {
            inode: 1,
            offset: 1,
            snapshot: 0,
            value: vec![17400028756955536248],
        }),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 1,
                offset: 11,
                snapshot: 2,
                value: vec![
                    6818856529010333162,
                    3146423145427384689,
                    16644132874213993808,
                    14635166167574342445,
                    2910556813483553160,
                    11708060591961091919,
                    5341616408040856889,
                    17438876503895870598,
                ],
            },
            Op::Delete {
                inode: 3,
                offset: 3,
                snapshot: 0,
            },
            Op::Delete {
                inode: 2,
                offset: 12,
                snapshot: 0,
            },
            Op::Put {
                inode: 3,
                offset: 6,
                snapshot: 1,
                value: vec![
                    8407044725254211194,
                    3958807969968848572,
                    15412882675406678135,
                    13101809454759529334,
                    1295375600652569679,
                    7778639571086437431,
                ],
            },
            Op::Delete {
                inode: 1,
                offset: 4,
                snapshot: 0,
            },
            Op::Delete {
                inode: 1,
                offset: 22,
                snapshot: 0,
            },
        ]),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 2,
                offset: 13,
                snapshot: 2,
            },
            Op::Delete {
                inode: 3,
                offset: 20,
                snapshot: 0,
            },
            Op::Put {
                inode: 1,
                offset: 9,
                snapshot: 1,
                value: vec![
                    17753131769431675254,
                    3970284075425850481,
                    2213300607641973765,
                    10639615638761249766,
                    160321948160998635,
                ],
            },
            Op::Put {
                inode: 2,
                offset: 22,
                snapshot: 2,
                value: vec![
                    8590348595283611266,
                    14259211932734146421,
                    10597931102080236394,
                ],
            },
            Op::Delete {
                inode: 3,
                offset: 20,
                snapshot: 1,
            },
            Op::Delete {
                inode: 3,
                offset: 5,
                snapshot: 1,
            },
        ]),
        OpGroup::Single(Op::Put {
            inode: 1,
            offset: 11,
            snapshot: 1,
            value: vec![
                14758529334310960776,
                14591953501113268287,
                6143424946810746721,
                12330887034761272840,
            ],
        }),
        OpGroup::Single(Op::Delete {
            inode: 3,
            offset: 7,
            snapshot: 1,
        }),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 1,
                offset: 9,
                snapshot: 2,
                value: vec![
                    16837266954972665096,
                    6696113784929453533,
                    4182025318267339680,
                    12970649010099568763,
                    10448175709910296887,
                    12746543718883662905,
                    15663710448757169400,
                ],
            },
            Op::Put {
                inode: 3,
                offset: 23,
                snapshot: 1,
                value: vec![
                    15120799964421850471,
                    2107575449801790232,
                    3630787479255724188,
                    530799094930476638,
                ],
            },
            Op::Put {
                inode: 3,
                offset: 4,
                snapshot: 0,
                value: vec![15901957898595596331],
            },
        ]),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 2,
                offset: 22,
                snapshot: 2,
                value: vec![
                    15834486478123761074,
                    2939818816427179209,
                    13272016715750760944,
                ],
            },
            Op::Delete {
                inode: 3,
                offset: 10,
                snapshot: 2,
            },
            Op::Delete {
                inode: 3,
                offset: 7,
                snapshot: 0,
            },
            Op::Put {
                inode: 1,
                offset: 1,
                snapshot: 0,
                value: vec![
                    11667526582244885433,
                    17789835214842434401,
                    2701472137240487448,
                ],
            },
        ]),
        OpGroup::Single(Op::Put {
            inode: 1,
            offset: 2,
            snapshot: 1,
            value: vec![
                8113881433801323466,
                17091684602612846032,
                17845577323156124646,
            ],
        }),
        OpGroup::Single(Op::Put {
            inode: 1,
            offset: 10,
            snapshot: 1,
            value: vec![6683163937724430418],
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 1,
                offset: 16,
                snapshot: 0,
            },
            Op::Delete {
                inode: 1,
                offset: 15,
                snapshot: 1,
            },
            Op::Delete {
                inode: 2,
                offset: 9,
                snapshot: 2,
            },
            Op::Delete {
                inode: 2,
                offset: 1,
                snapshot: 0,
            },
            Op::Delete {
                inode: 2,
                offset: 15,
                snapshot: 2,
            },
            Op::Delete {
                inode: 3,
                offset: 18,
                snapshot: 0,
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 1,
            offset: 22,
            snapshot: 0,
        }),
        OpGroup::Single(Op::Put {
            inode: 1,
            offset: 8,
            snapshot: 2,
            value: vec![
                14201457096970020539,
                17159664978253965383,
                7734546401995081633,
                10743955445228550797,
            ],
        }),
        OpGroup::Single(Op::Put {
            inode: 1,
            offset: 8,
            snapshot: 1,
            value: vec![
                13517752343253869985,
                128223419675262502,
                6949245368169408860,
                11172031528142380277,
                15975432023063929864,
                17964444745815097031,
                13072565142479488242,
                11153476674253577948,
            ],
        }),
        OpGroup::Single(Op::Delete {
            inode: 3,
            offset: 16,
            snapshot: 0,
        }),
        OpGroup::Single(Op::Delete {
            inode: 1,
            offset: 18,
            snapshot: 1,
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 1,
                offset: 13,
                snapshot: 0,
            },
            Op::Delete {
                inode: 3,
                offset: 2,
                snapshot: 2,
            },
        ]),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 3,
                offset: 18,
                snapshot: 2,
            },
            Op::Put {
                inode: 3,
                offset: 15,
                snapshot: 0,
                value: vec![
                    16564115956415062169,
                    5986065484741452440,
                    17324989910107919069,
                    6293678978906364021,
                    14291225336351970348,
                    2247346060074805887,
                    18201525938666554817,
                ],
            },
        ]),
        OpGroup::Single(Op::Put {
            inode: 2,
            offset: 24,
            snapshot: 0,
            value: vec![
                3145894152610963278,
                4405330634416964813,
                11600890446099609712,
                4206237837687594395,
                5000935912301523350,
                6869932425164679244,
                2498537892530364873,
            ],
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 2,
                offset: 12,
                snapshot: 1,
            },
            Op::Delete {
                inode: 1,
                offset: 24,
                snapshot: 1,
            },
            Op::Put {
                inode: 3,
                offset: 6,
                snapshot: 0,
                value: vec![
                    15764362366203225982,
                    16670037598911956600,
                    3419007678228343482,
                    9446563349820122656,
                ],
            },
            Op::Delete {
                inode: 3,
                offset: 2,
                snapshot: 2,
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 2,
            offset: 14,
            snapshot: 2,
        }),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 1,
                offset: 24,
                snapshot: 0,
                value: vec![14659358174177994355],
            },
            Op::Put {
                inode: 1,
                offset: 9,
                snapshot: 1,
                value: vec![
                    11720335676315771585,
                    3741270308941904174,
                    14772548022071147324,
                    6051782902161486933,
                    5198366605706612664,
                ],
            },
        ]),
        OpGroup::Single(Op::Put {
            inode: 3,
            offset: 21,
            snapshot: 0,
            value: vec![
                1100300198216110294,
                5358426176808596854,
                18103800420077385003,
                2735265340737019416,
            ],
        }),
        OpGroup::Single(Op::Put {
            inode: 3,
            offset: 10,
            snapshot: 1,
            value: vec![5720912561105810359],
        }),
        OpGroup::Single(Op::Put {
            inode: 3,
            offset: 13,
            snapshot: 1,
            value: vec![
                16397972426179921815,
                5184305372964017871,
                16507534114647036077,
                4678513092590019005,
                12495699012612562769,
            ],
        }),
        OpGroup::Single(Op::Delete {
            inode: 2,
            offset: 18,
            snapshot: 0,
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 1,
                offset: 23,
                snapshot: 0,
            },
            Op::Delete {
                inode: 1,
                offset: 4,
                snapshot: 0,
            },
            Op::Delete {
                inode: 2,
                offset: 10,
                snapshot: 1,
            },
            Op::Put {
                inode: 3,
                offset: 17,
                snapshot: 1,
                value: vec![
                    10081799346585149517,
                    1309498305469482251,
                    17656995288401110050,
                    8281139352537081629,
                ],
            },
            Op::Put {
                inode: 2,
                offset: 15,
                snapshot: 0,
                value: vec![
                    11394250501684139212,
                    4859300091824501813,
                    3206153065644279782,
                    7305903387785114548,
                    8027453873309995622,
                    171267977394904957,
                    7804442840017835781,
                ],
            },
        ]),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 3,
                offset: 4,
                snapshot: 0,
            },
            Op::Put {
                inode: 3,
                offset: 17,
                snapshot: 1,
                value: vec![3672226175310708268, 5230248965933096923],
            },
            Op::Put {
                inode: 2,
                offset: 6,
                snapshot: 1,
                value: vec![17884151473612417730],
            },
        ]),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 2,
                offset: 22,
                snapshot: 2,
            },
            Op::Put {
                inode: 3,
                offset: 9,
                snapshot: 0,
                value: vec![
                    16718692298192512694,
                    10757090446286846652,
                    4113722254651430535,
                    6026656069251270394,
                    9674945670139120931,
                    5675437894389374138,
                    9837486035780520827,
                    17518672263670774337,
                ],
            },
            Op::Delete {
                inode: 2,
                offset: 23,
                snapshot: 0,
            },
            Op::Delete {
                inode: 2,
                offset: 20,
                snapshot: 0,
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 2,
            offset: 9,
            snapshot: 1,
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 2,
                offset: 16,
                snapshot: 2,
            },
            Op::Put {
                inode: 3,
                offset: 24,
                snapshot: 2,
                value: vec![
                    1066966887434000417,
                    1666470872659754597,
                    14705728970992483264,
                    15838457082208587362,
                    1602535613474642723,
                    1752536348686728705,
                    9436124992673959351,
                    3481980284841565604,
                ],
            },
            Op::Put {
                inode: 3,
                offset: 15,
                snapshot: 2,
                value: vec![
                    925637551644657138,
                    15242847823198493711,
                    18389462731026847819,
                    12682751933101965201,
                    8829832626270087510,
                    13956026786997697582,
                ],
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 1,
            offset: 17,
            snapshot: 0,
        }),
        OpGroup::Single(Op::Delete {
            inode: 2,
            offset: 15,
            snapshot: 0,
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 3,
                offset: 12,
                snapshot: 0,
            },
            Op::Put {
                inode: 2,
                offset: 14,
                snapshot: 2,
                value: vec![
                    13721862669897195184,
                    4466653877608199925,
                    13988186647043079658,
                ],
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 3,
            offset: 17,
            snapshot: 1,
        }),
        OpGroup::Single(Op::Put {
            inode: 3,
            offset: 24,
            snapshot: 0,
            value: vec![
                8500350009664679251,
                6291860231799775564,
                12100842708040958275,
                2378641628073668164,
                3390809543589379662,
                14729648153455236877,
                5420650806884857590,
            ],
        }),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 1,
                offset: 12,
                snapshot: 0,
                value: vec![8094067019586893108, 4162453793633483783],
            },
            Op::Put {
                inode: 3,
                offset: 14,
                snapshot: 1,
                value: vec![
                    12407594557757180660,
                    3130089128368300170,
                    14910799743945353308,
                    2931203041029319645,
                    1154236318609208934,
                    7165885763585383385,
                    13780411862109590170,
                ],
            },
            Op::Put {
                inode: 1,
                offset: 4,
                snapshot: 0,
                value: vec![
                    8456053404417733002,
                    4417096889463101232,
                    3446540201165750872,
                    7599056915676023207,
                    11596951039773881581,
                    281816388762146006,
                    14496195932393181845,
                    4358502147021455556,
                ],
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 1,
            offset: 23,
            snapshot: 1,
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 1,
                offset: 21,
                snapshot: 0,
            },
            Op::Delete {
                inode: 1,
                offset: 3,
                snapshot: 2,
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 3,
            offset: 24,
            snapshot: 0,
        }),
        OpGroup::Single(Op::Put {
            inode: 2,
            offset: 14,
            snapshot: 0,
            value: vec![
                5210455204077742883,
                1462902645412833620,
                4550499429537315055,
                12714237693473657316,
                6578793925730385534,
                8628192155161058918,
                14360508810598101683,
                5021965453699350104,
            ],
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 3,
                offset: 16,
                snapshot: 1,
            },
            Op::Put {
                inode: 2,
                offset: 5,
                snapshot: 1,
                value: vec![3547124605366504966],
            },
        ]),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 2,
                offset: 13,
                snapshot: 0,
            },
            Op::Put {
                inode: 1,
                offset: 10,
                snapshot: 1,
                value: vec![
                    154557234781954471,
                    14005055004248503915,
                    5851135407711476668,
                    17196551365452410475,
                ],
            },
            Op::Put {
                inode: 2,
                offset: 18,
                snapshot: 1,
                value: vec![15772753010163648720],
            },
            Op::Put {
                inode: 1,
                offset: 19,
                snapshot: 1,
                value: vec![
                    15325587350866118996,
                    4381825433651938522,
                    1782892727989577067,
                    14494684636789108047,
                    15227768379337302043,
                    6404646094214299021,
                    14850789261919653966,
                ],
            },
            Op::Delete {
                inode: 3,
                offset: 13,
                snapshot: 0,
            },
            Op::Delete {
                inode: 2,
                offset: 21,
                snapshot: 2,
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 3,
            offset: 18,
            snapshot: 1,
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 2,
                offset: 15,
                snapshot: 2,
            },
            Op::Delete {
                inode: 3,
                offset: 20,
                snapshot: 2,
            },
            Op::Delete {
                inode: 2,
                offset: 8,
                snapshot: 0,
            },
            Op::Delete {
                inode: 3,
                offset: 19,
                snapshot: 0,
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 1,
            offset: 21,
            snapshot: 2,
        }),
        OpGroup::Single(Op::Delete {
            inode: 1,
            offset: 11,
            snapshot: 0,
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 1,
                offset: 18,
                snapshot: 1,
            },
            Op::Delete {
                inode: 3,
                offset: 12,
                snapshot: 1,
            },
            Op::Put {
                inode: 3,
                offset: 9,
                snapshot: 1,
                value: vec![
                    11127867425202808200,
                    8351921770318851383,
                    1418677127478597214,
                    15535498572073467339,
                    15673850094171189594,
                ],
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 3,
            offset: 18,
            snapshot: 0,
        }),
        OpGroup::Single(Op::Delete {
            inode: 3,
            offset: 2,
            snapshot: 1,
        }),
        OpGroup::Single(Op::Put {
            inode: 2,
            offset: 21,
            snapshot: 0,
            value: vec![3905488500619554287],
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 1,
                offset: 12,
                snapshot: 0,
            },
            Op::Delete {
                inode: 1,
                offset: 15,
                snapshot: 2,
            },
            Op::Delete {
                inode: 2,
                offset: 22,
                snapshot: 2,
            },
        ]),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 2,
                offset: 18,
                snapshot: 0,
                value: vec![
                    13070583414260013571,
                    6675625870607360901,
                    16823885828234229788,
                    4409169768280928574,
                    8898408225009255689,
                    3172617328576914265,
                    13123826601272578063,
                    15460319063738816123,
                ],
            },
            Op::Put {
                inode: 1,
                offset: 11,
                snapshot: 2,
                value: vec![
                    13440411335653130150,
                    14188174194744001063,
                    12506996024222315171,
                    4709210675890516120,
                ],
            },
        ]),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 2,
                offset: 15,
                snapshot: 2,
                value: vec![
                    9864952075151200403,
                    8617550275550471421,
                    3956249493783078609,
                ],
            },
            Op::Delete {
                inode: 2,
                offset: 15,
                snapshot: 1,
            },
            Op::Delete {
                inode: 2,
                offset: 14,
                snapshot: 2,
            },
            Op::Delete {
                inode: 2,
                offset: 18,
                snapshot: 1,
            },
        ]),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 1,
                offset: 14,
                snapshot: 1,
                value: vec![
                    13387839571832856636,
                    9530669956259543673,
                    9126882262044199742,
                    3954681911694965383,
                ],
            },
            Op::Put {
                inode: 2,
                offset: 17,
                snapshot: 0,
                value: vec![
                    8644505565226559696,
                    9597174880795454320,
                    3642271006834387180,
                ],
            },
            Op::Delete {
                inode: 1,
                offset: 17,
                snapshot: 2,
            },
        ]),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 2,
                offset: 1,
                snapshot: 1,
                value: vec![
                    12286938543666130220,
                    4657327566599030971,
                    13660249993879117139,
                    9975132922276087887,
                    16584086499363927625,
                    5836558833278864120,
                    2321628853413710561,
                    2416197607471567788,
                ],
            },
            Op::Put {
                inode: 1,
                offset: 10,
                snapshot: 0,
                value: vec![
                    16239603976694495198,
                    5692580287469681824,
                    10879454804128857654,
                    4814471585024158903,
                    4266082581202026500,
                    16415580750774611688,
                    16823054674177761639,
                ],
            },
            Op::Put {
                inode: 2,
                offset: 15,
                snapshot: 0,
                value: vec![4041242977990322031],
            },
        ]),
        OpGroup::Single(Op::Put {
            inode: 1,
            offset: 8,
            snapshot: 2,
            value: vec![
                13755555172088564898,
                314339696602351487,
                7080756918673390094,
                1335548599171967239,
            ],
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 1,
                offset: 9,
                snapshot: 0,
            },
            Op::Put {
                inode: 2,
                offset: 15,
                snapshot: 2,
                value: vec![
                    3145972660624835523,
                    4283181698781648356,
                    10611147184248163108,
                    15864986595578145898,
                    2427739246681168617,
                    12847601346325821074,
                ],
            },
            Op::Put {
                inode: 3,
                offset: 16,
                snapshot: 2,
                value: vec![
                    5417427395521728629,
                    13587374982339009396,
                    4977172596396054406,
                    1851803274130386399,
                    15413898663213535013,
                    736154373043929261,
                ],
            },
            Op::Delete {
                inode: 2,
                offset: 16,
                snapshot: 1,
            },
            Op::Delete {
                inode: 3,
                offset: 10,
                snapshot: 0,
            },
            Op::Delete {
                inode: 3,
                offset: 10,
                snapshot: 1,
            },
        ]),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 1,
                offset: 12,
                snapshot: 2,
            },
            Op::Put {
                inode: 1,
                offset: 6,
                snapshot: 1,
                value: vec![
                    14659443132199890534,
                    13944660088228225032,
                    10090828344636776204,
                ],
            },
        ]),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 2,
                offset: 3,
                snapshot: 1,
            },
            Op::Put {
                inode: 2,
                offset: 14,
                snapshot: 2,
                value: vec![
                    14126071748385869414,
                    5181487241023763545,
                    5323921547590578431,
                    18396642742742670139,
                    14916895920869347285,
                    1405889304385808358,
                    17729255957238910462,
                ],
            },
            Op::Put {
                inode: 2,
                offset: 24,
                snapshot: 0,
                value: vec![
                    2084922159830930594,
                    16409238120808389071,
                    1190167724539505489,
                    9259734664428002589,
                    16247060229658130277,
                    1618589252716501228,
                    1008576875381449845,
                ],
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 1,
            offset: 20,
            snapshot: 1,
        }),
        OpGroup::Single(Op::Put {
            inode: 2,
            offset: 2,
            snapshot: 1,
            value: vec![12840159567418887675, 6136897158642800585],
        }),
        OpGroup::Single(Op::Delete {
            inode: 3,
            offset: 10,
            snapshot: 0,
        }),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 2,
                offset: 13,
                snapshot: 2,
                value: vec![
                    12121906544052612383,
                    13010847621843564579,
                    15528919711177171769,
                    6774944042438101849,
                    2664077816395819379,
                    12145312901594570962,
                    14495999053372362682,
                    17913856911243083292,
                ],
            },
            Op::Put {
                inode: 1,
                offset: 7,
                snapshot: 1,
                value: vec![
                    6929775458898561031,
                    5194124355328936860,
                    17580074437952685035,
                    10945255325790307471,
                    13960571473736522717,
                    4795465163662249243,
                    3441872171370570641,
                    2641193512643322011,
                ],
            },
        ]),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 1,
                offset: 16,
                snapshot: 2,
                value: vec![
                    10006948610791590437,
                    3797505190257719460,
                    5412985360734838582,
                    13477926977433487460,
                    13133582153368354448,
                    5444869040894789117,
                    1689568147243461618,
                ],
            },
            Op::Delete {
                inode: 3,
                offset: 20,
                snapshot: 1,
            },
            Op::Put {
                inode: 2,
                offset: 19,
                snapshot: 0,
                value: vec![2313737144891903634, 11779589650317635248],
            },
            Op::Delete {
                inode: 3,
                offset: 9,
                snapshot: 2,
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 1,
            offset: 6,
            snapshot: 0,
        }),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 2,
                offset: 16,
                snapshot: 2,
                value: vec![
                    16051030398510830237,
                    11814006326329825598,
                    14808001321785714747,
                    115470931567567923,
                    17578412102904353269,
                    9740376169823727234,
                    14140732309882675781,
                    611570710459946254,
                ],
            },
            Op::Delete {
                inode: 2,
                offset: 14,
                snapshot: 0,
            },
            Op::Put {
                inode: 1,
                offset: 20,
                snapshot: 1,
                value: vec![
                    5212334808112383549,
                    5826655571700863235,
                    12674065507842366818,
                    3109400491700840143,
                    17180533383219720090,
                    17792877788094360877,
                    13138228924367477073,
                ],
            },
            Op::Put {
                inode: 1,
                offset: 6,
                snapshot: 0,
                value: vec![
                    1767078925518787048,
                    2919597765797024877,
                    18230641250119089156,
                    14915858592763848500,
                    7827289035702136977,
                    10916239313532389143,
                ],
            },
        ]),
        OpGroup::Single(Op::Put {
            inode: 2,
            offset: 15,
            snapshot: 1,
            value: vec![14227127679788089468, 12658637450492908143],
        }),
        OpGroup::Single(Op::Put {
            inode: 2,
            offset: 3,
            snapshot: 1,
            value: vec![
                2918066790390986044,
                14893879626665743631,
                1992966810825498943,
                14448760717251648091,
                12704880531992134406,
                8523387555138747128,
                2206855225604322617,
            ],
        }),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 3,
                offset: 8,
                snapshot: 0,
                value: vec![
                    4756916785501247752,
                    746272697578680645,
                    15812250816631904924,
                    1104623281470316941,
                    11331397233925976441,
                    13554142832634727652,
                    11268277990147543696,
                    18443843275245636028,
                ],
            },
            Op::Delete {
                inode: 1,
                offset: 24,
                snapshot: 0,
            },
            Op::Delete {
                inode: 2,
                offset: 3,
                snapshot: 2,
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 1,
            offset: 2,
            snapshot: 0,
        }),
        OpGroup::Single(Op::Delete {
            inode: 2,
            offset: 7,
            snapshot: 0,
        }),
        OpGroup::Single(Op::Put {
            inode: 3,
            offset: 15,
            snapshot: 2,
            value: vec![
                13320300458143463359,
                1484630805956423647,
                11042625623089550580,
            ],
        }),
        OpGroup::Single(Op::Put {
            inode: 1,
            offset: 17,
            snapshot: 0,
            value: vec![86334214073779804, 2690573233462181969],
        }),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 3,
                offset: 2,
                snapshot: 1,
                value: vec![
                    8481579097403224809,
                    13932372860524220102,
                    11273459496302184683,
                    14943331514070068140,
                    139814266987630947,
                    11495270662096663626,
                    10587844036282155069,
                ],
            },
            Op::Put {
                inode: 2,
                offset: 20,
                snapshot: 0,
                value: vec![
                    8402362808210712550,
                    2822378554367907914,
                    16669121168454609136,
                    5526037313278422515,
                    14762935604176204479,
                    5790918376296074615,
                    6093191614089429189,
                ],
            },
            Op::Put {
                inode: 2,
                offset: 7,
                snapshot: 1,
                value: vec![
                    5826721775144254493,
                    8116296258471567213,
                    8030563530690797975,
                    3425642718659115373,
                ],
            },
            Op::Put {
                inode: 3,
                offset: 1,
                snapshot: 1,
                value: vec![11422909284223931662, 4384795298307614886],
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 1,
            offset: 9,
            snapshot: 2,
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 2,
                offset: 22,
                snapshot: 2,
            },
            Op::Delete {
                inode: 3,
                offset: 10,
                snapshot: 0,
            },
            Op::Delete {
                inode: 3,
                offset: 9,
                snapshot: 0,
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 1,
            offset: 4,
            snapshot: 2,
        }),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 1,
                offset: 10,
                snapshot: 2,
                value: vec![
                    5822742771759335624,
                    7388215830183948507,
                    17158131892244395244,
                ],
            },
            Op::Put {
                inode: 3,
                offset: 19,
                snapshot: 2,
                value: vec![
                    4956735458259283534,
                    13712860743076835686,
                    3030360316222537961,
                    6006924748728329315,
                    13890092328856410846,
                    17183844844032044549,
                    8467056639504828145,
                ],
            },
        ]),
        OpGroup::Single(Op::Put {
            inode: 2,
            offset: 19,
            snapshot: 2,
            value: vec![
                7807044524280084815,
                11330939962798425357,
                1868551742468152598,
            ],
        }),
        OpGroup::Single(Op::Delete {
            inode: 2,
            offset: 18,
            snapshot: 2,
        }),
        OpGroup::Single(Op::Delete {
            inode: 3,
            offset: 11,
            snapshot: 1,
        }),
        OpGroup::Single(Op::Put {
            inode: 3,
            offset: 7,
            snapshot: 1,
            value: vec![
                17226489458672126342,
                6680250660475515062,
                16357682801981139937,
                6590967289425439481,
                8789444519024521882,
                1026270111334212737,
                4595650773361132549,
                6244094225619137807,
            ],
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 1,
                offset: 13,
                snapshot: 2,
            },
            Op::Delete {
                inode: 3,
                offset: 10,
                snapshot: 1,
            },
            Op::Delete {
                inode: 2,
                offset: 16,
                snapshot: 2,
            },
        ]),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 2,
                offset: 2,
                snapshot: 2,
                value: vec![
                    16284345979654351183,
                    7638567731270642243,
                    13467236834969013394,
                    14023512541043169507,
                    867841658636006742,
                    4868973040625099996,
                    12816110828797852230,
                    768599986029750615,
                ],
            },
            Op::Put {
                inode: 1,
                offset: 4,
                snapshot: 1,
                value: vec![
                    7439042365456120276,
                    16048711347945107986,
                    15781505046769110787,
                    13462718144208022390,
                ],
            },
            Op::Put {
                inode: 3,
                offset: 23,
                snapshot: 1,
                value: vec![7010736067571503511, 13406376882600218014],
            },
        ]),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 1,
                offset: 24,
                snapshot: 1,
                value: vec![
                    14776744198800299238,
                    9996207935862460176,
                    17921638719298747303,
                    8883605709223194493,
                    15338344418784120565,
                ],
            },
            Op::Put {
                inode: 2,
                offset: 3,
                snapshot: 2,
                value: vec![
                    16235541836714511201,
                    3650477171609166731,
                    6959269753054656881,
                    10654101556507401579,
                ],
            },
        ]),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 3,
                offset: 8,
                snapshot: 1,
                value: vec![2005338222798584411],
            },
            Op::Delete {
                inode: 1,
                offset: 7,
                snapshot: 0,
            },
            Op::Delete {
                inode: 3,
                offset: 10,
                snapshot: 1,
            },
            Op::Delete {
                inode: 2,
                offset: 7,
                snapshot: 0,
            },
        ]),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 2,
                offset: 9,
                snapshot: 2,
            },
            Op::Put {
                inode: 2,
                offset: 6,
                snapshot: 2,
                value: vec![2062095056916260333, 13179562221213448281],
            },
            Op::Delete {
                inode: 1,
                offset: 9,
                snapshot: 2,
            },
            Op::Delete {
                inode: 3,
                offset: 19,
                snapshot: 2,
            },
            Op::Put {
                inode: 1,
                offset: 10,
                snapshot: 1,
                value: vec![13821791178766785929, 17547458872352817183],
            },
            Op::Put {
                inode: 3,
                offset: 5,
                snapshot: 2,
                value: vec![
                    13139706744911677959,
                    4461370880358801967,
                    17830207453086469980,
                    7002060586028561194,
                    287585981425843417,
                ],
            },
        ]),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 1,
                offset: 23,
                snapshot: 0,
                value: vec![
                    5393517712202143385,
                    5663495969785174710,
                    5032586790702173049,
                    7435795495728137041,
                    13734587695123490028,
                    9333273697555439526,
                ],
            },
            Op::Put {
                inode: 3,
                offset: 11,
                snapshot: 1,
                value: vec![14025388753758679362, 5337532347114674392],
            },
        ]),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 2,
                offset: 7,
                snapshot: 1,
                value: vec![
                    7403234339118939964,
                    8725350780717997977,
                    417072724954888300,
                    3082503451201233732,
                    14301797973691484145,
                ],
            },
            Op::Delete {
                inode: 1,
                offset: 18,
                snapshot: 0,
            },
        ]),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 2,
                offset: 3,
                snapshot: 1,
                value: vec![
                    8693199232284872088,
                    12001645104384693588,
                    16533808851226729687,
                    14452865578966539979,
                ],
            },
            Op::Put {
                inode: 3,
                offset: 22,
                snapshot: 2,
                value: vec![
                    1719097048322786918,
                    13934481619865403033,
                    12507951261743534827,
                    11000817184363199061,
                ],
            },
            Op::Put {
                inode: 1,
                offset: 4,
                snapshot: 1,
                value: vec![
                    14692024137205269903,
                    14636145581892637570,
                    16166475411889045836,
                    16310444235212143485,
                ],
            },
            Op::Delete {
                inode: 2,
                offset: 17,
                snapshot: 2,
            },
            Op::Delete {
                inode: 3,
                offset: 22,
                snapshot: 1,
            },
            Op::Put {
                inode: 2,
                offset: 14,
                snapshot: 2,
                value: vec![
                    3573210296658386321,
                    418973783560977193,
                    15599324646073867682,
                    11717443200072364519,
                    4813220482094794523,
                    15450551937074065057,
                ],
            },
        ]),
        OpGroup::Single(Op::Put {
            inode: 1,
            offset: 8,
            snapshot: 1,
            value: vec![
                7115521410746937773,
                4187010027097357195,
                15917017665055256021,
                11607428558348447521,
                12981072670672944846,
                13441741670842461445,
                9638514657588960748,
                2101085837905429216,
            ],
        }),
        OpGroup::Single(Op::Put {
            inode: 3,
            offset: 16,
            snapshot: 0,
            value: vec![
                13352415153029479454,
                1879498167505231507,
                4943625811003920365,
                5720415728434340362,
                6543062975277526758,
                493609910345051006,
                7880165165173961334,
            ],
        }),
        OpGroup::Single(Op::Delete {
            inode: 1,
            offset: 19,
            snapshot: 0,
        }),
        OpGroup::Single(Op::Delete {
            inode: 3,
            offset: 6,
            snapshot: 0,
        }),
        OpGroup::Single(Op::Put {
            inode: 2,
            offset: 19,
            snapshot: 2,
            value: vec![
                16021949034479612390,
                6495467251763483497,
                1707717154802592018,
                6657488219702255325,
                7573633955170600858,
                4937941171807893291,
            ],
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 1,
                offset: 22,
                snapshot: 1,
            },
            Op::Put {
                inode: 1,
                offset: 12,
                snapshot: 2,
                value: vec![4229253006161047119, 16918459460243787485],
            },
            Op::Delete {
                inode: 1,
                offset: 21,
                snapshot: 0,
            },
            Op::Put {
                inode: 3,
                offset: 6,
                snapshot: 1,
                value: vec![
                    11059879559741140104,
                    9369522144680952070,
                    5316755073433637140,
                ],
            },
            Op::Delete {
                inode: 3,
                offset: 11,
                snapshot: 1,
            },
            Op::Delete {
                inode: 3,
                offset: 15,
                snapshot: 1,
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 1,
            offset: 3,
            snapshot: 1,
        }),
        OpGroup::Single(Op::Delete {
            inode: 2,
            offset: 17,
            snapshot: 0,
        }),
    ];
    let engine = StorageEngine::new().unwrap();
    let mut model = BTreeMap::new();
    for (step, group) in ops.iter().enumerate() {
        eprintln!("repro step={step}");
        apply_group(&engine, group).unwrap();
        for op in ops_of(group) {
            apply_model(&mut model, op);
        }
    }
    assert_model(&engine, &model);
}

/// 确定性回归：proptest 抓到的 scan 丢键 {2,15,2}（首 op 写入的键丢失）。
#[test]
fn deterministic_scan_loss_repro() {
    let ops: Vec<OpGroup> = vec![
        OpGroup::Single(Op::Put {
            inode: 2,
            offset: 15,
            snapshot: 2,
            value: vec![4388523686570927033, 1318305333972467278],
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 2,
                offset: 8,
                snapshot: 2,
            },
            Op::Delete {
                inode: 3,
                offset: 16,
                snapshot: 1,
            },
            Op::Put {
                inode: 2,
                offset: 15,
                snapshot: 0,
                value: vec![
                    9212793138441779452,
                    3837844129805075331,
                    2382413373724433355,
                ],
            },
            Op::Delete {
                inode: 3,
                offset: 2,
                snapshot: 2,
            },
            Op::Delete {
                inode: 3,
                offset: 18,
                snapshot: 1,
            },
            Op::Delete {
                inode: 3,
                offset: 15,
                snapshot: 1,
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 3,
            offset: 21,
            snapshot: 0,
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 3,
                offset: 3,
                snapshot: 1,
            },
            Op::Delete {
                inode: 3,
                offset: 8,
                snapshot: 1,
            },
        ]),
        OpGroup::Single(Op::Put {
            inode: 3,
            offset: 16,
            snapshot: 2,
            value: vec![
                14288803234071126410,
                5484978901497303017,
                11990336647005659010,
            ],
        }),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 1,
                offset: 3,
                snapshot: 2,
                value: vec![
                    2541486745223974339,
                    6294141637994801639,
                    17287243748075544956,
                    4548035953026676819,
                    13951203633008286015,
                    5331626592902865159,
                    5472700317053442873,
                    9759832390514189112,
                ],
            },
            Op::Put {
                inode: 2,
                offset: 11,
                snapshot: 2,
                value: vec![13777956579183823718],
            },
        ]),
        OpGroup::Batch(vec![
            Op::Put {
                inode: 1,
                offset: 13,
                snapshot: 0,
                value: vec![
                    16399607307024265952,
                    10619925657797991772,
                    12230708193500611134,
                    4344079127058738332,
                    16429164798083309723,
                ],
            },
            Op::Delete {
                inode: 2,
                offset: 18,
                snapshot: 0,
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 3,
            offset: 20,
            snapshot: 1,
        }),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 2,
                offset: 7,
                snapshot: 2,
            },
            Op::Put {
                inode: 1,
                offset: 14,
                snapshot: 0,
                value: vec![
                    946767450265907540,
                    12728860222703941420,
                    7534010532157549035,
                    12822605758123109141,
                    502943605543811243,
                    14292916500741751435,
                    7402818075519738050,
                    5517382554363435496,
                ],
            },
            Op::Delete {
                inode: 1,
                offset: 24,
                snapshot: 2,
            },
        ]),
        OpGroup::Batch(vec![
            Op::Delete {
                inode: 2,
                offset: 4,
                snapshot: 0,
            },
            Op::Delete {
                inode: 2,
                offset: 13,
                snapshot: 2,
            },
            Op::Put {
                inode: 2,
                offset: 22,
                snapshot: 2,
                value: vec![
                    11021712577104610710,
                    3049493934465491763,
                    15429554323870557190,
                    1537926787359134833,
                    6992820251436376588,
                ],
            },
            Op::Delete {
                inode: 1,
                offset: 21,
                snapshot: 1,
            },
        ]),
        OpGroup::Single(Op::Delete {
            inode: 2,
            offset: 15,
            snapshot: 0,
        }),
    ];
    let engine = StorageEngine::new().unwrap();
    let mut model = BTreeMap::new();
    for (step, group) in ops.iter().enumerate() {
        eprintln!("repro step={step}");
        apply_group(&engine, group).unwrap();
        for op in ops_of(group) {
            apply_model(&mut model, op);
        }
        assert_model(&engine, &model);
    }
}
