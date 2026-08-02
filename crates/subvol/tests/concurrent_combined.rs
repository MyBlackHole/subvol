//! T0203 并发组合序列：多写者 × alloc op × 崩溃恢复精确断言。
//!
//! 组合 T0202（组合 op 域模型：btree × alloc 4 op × 崩溃恢复精确断言）
//! 与 T0201（多线程写者 × 确定性崩溃点 abort）：
//!
//! - 多写者线程（3）各自执行组合 op（put/delete/allocate/reclaim/
//!   queue_discard/run_discard_worker_once），每次 op 在测试锁内完成
//!   "引擎提交 + 提交日志追加"，因此日志顺序 == 引擎提交顺序（全局
//!   fs 锁串行化的真实顺序），日志是精确性的确定性来源。
//! - 崩溃点：全部写者完成 → 提交日志落盘（sync_all）→ engine.sync()
//!   （journal 落盘）→ abort。恢复后已提交 op 全部生效，未提交零。
//! - 父进程按提交日志重放 BucketModel（T0202 三态 + VecDeque 队列 +
//!   btree 模型），open_persistent 后做**精确**断言（非 T0201 的
//!   最终一致）。
//!
//! bcachefs 语义锚点（T0195/T0201/T0202 已核对，本任务复核）：
//! - journal replay 只回放已落盘记录（journal/read.c
//!   journal_replay_maybe_drop_overwrites，seq_ondisk 边界）；
//! - 崩溃 = abort 不 flush（engine.rs:1801-1836），恢复 =
//!   replay + rebuild_derived_state；
//! - alloc op 语义（allocate/reclaim/queue/worker）由 T0202 锚点表
//!   固定（foreground.c 候选规则、discard.c:643 darray、
//!   discover 重入队 engine.rs:1309）；
//! - 提交串行化 = 全局 fs 锁（T0199 并发矩阵实测）。
//!
//! 模型是项目自有的验证设施，不构成运行时逻辑。

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use proptest::prelude::*;
use subvol::{BtreeId, BtreeKey, EngineError, FaultPoint, KeyPosition, StorageEngine};

const CASES: u32 = 10;
const WRITERS: usize = 3;

/* 桶域：4 个 free 桶（4..=7），对齐 T0202 组合测试与 T0201 并发框架。 */
const BUCKET_OFFSETS: [u64; 4] = [4, 5, 6, 7];
const N_BUCKETS: usize = 4;

fn key_strategy() -> impl Strategy<Value = (u64, u64, u32)> {
    (1u64..=3, 1u64..=24, 0u32..=2)
}

fn value_strategy() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(any::<u64>(), 1..=8)
}

/// 并发组合计划：每个写者一个 op 列表。
#[derive(Clone, Debug)]
enum PlanOp {
    Put(u64, u64, u32, Vec<u64>),
    Delete(u64, u64, u32),
    Allocate,
    Reclaim(usize),
    Queue(usize),
    RunOnce,
}

fn plan_op_strategy() -> impl Strategy<Value = PlanOp> {
    prop_oneof![
        (key_strategy(), value_strategy()).prop_map(|((i, o, s), v)| PlanOp::Put(i, o, s, v)),
        key_strategy().prop_map(|(i, o, s)| PlanOp::Delete(i, o, s)),
        any::<u8>().prop_map(|_| PlanOp::Allocate),
        any::<u8>().prop_map(|_| PlanOp::Reclaim(0)),
        any::<u8>().prop_map(|_| PlanOp::Queue(0)),
        any::<u8>().prop_map(|_| PlanOp::RunOnce),
    ]
}

fn plan_strategy() -> impl Strategy<Value = Vec<Vec<PlanOp>>> {
    prop::collection::vec(prop::collection::vec(plan_op_strategy(), 6..=12), WRITERS)
}

fn unique_tmp_path(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "subvol-cc-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    path
}

fn encode_op(op: &PlanOp) -> String {
    match op {
        PlanOp::Put(i, o, s, v) => {
            let vals = v.iter().map(u64::to_string).collect::<Vec<_>>().join(" ");
            format!("P {i} {o} {s} {vals}")
        }
        PlanOp::Delete(i, o, s) => format!("D {i} {o} {s}"),
        PlanOp::Allocate => "A".to_string(),
        PlanOp::Reclaim(idx) => format!("R {idx}"),
        PlanOp::Queue(idx) => format!("Q {idx}"),
        PlanOp::RunOnce => "W".to_string(),
    }
}

fn decode_op(line: &str) -> PlanOp {
    let mut it = line.split_whitespace();
    match it.next().unwrap() {
        "P" => PlanOp::Put(
            it.next().unwrap().parse().unwrap(),
            it.next().unwrap().parse().unwrap(),
            it.next().unwrap().parse().unwrap(),
            it.map(|w| w.parse().unwrap()).collect(),
        ),
        "D" => PlanOp::Delete(
            it.next().unwrap().parse().unwrap(),
            it.next().unwrap().parse().unwrap(),
            it.next().unwrap().parse().unwrap(),
        ),
        "A" => PlanOp::Allocate,
        "R" => PlanOp::Reclaim(it.next().unwrap().parse().unwrap()),
        "Q" => PlanOp::Queue(it.next().unwrap().parse().unwrap()),
        "W" => PlanOp::RunOnce,
        other => panic!("unknown plan op {other}"),
    }
}

/// 执行单个 op：在测试锁内完成"引擎提交 + 日志追加"，返回结果编码。
/// 锁内执行保证日志顺序 == 引擎提交顺序（引擎 fs 锁串行化）。
fn execute_op(engine: &StorageEngine, op: &PlanOp) -> String {
    let result = match op {
        PlanOp::Put(i, o, s, v) => engine
            .put(
                BtreeId::DEFAULT,
                BtreeKey::new(KeyPosition::new(*i, *o, *s), v.clone()).unwrap(),
            )
            .map(|()| "ok".to_string()),
        PlanOp::Delete(i, o, s) => engine
            .delete(BtreeId::DEFAULT, KeyPosition::new(*i, *o, *s))
            .map(|()| "ok".to_string()),
        PlanOp::Allocate => engine
            .allocate_bucket(0)
            .map(|pos| format!("ok {} {}", pos.inode, pos.offset)),
        PlanOp::Reclaim(idx) => engine
            .reclaim_bucket(KeyPosition::new(0, BUCKET_OFFSETS[*idx], 0))
            .map(|()| "ok".to_string()),
        PlanOp::Queue(idx) => engine
            .queue_discard_bucket(KeyPosition::new(0, BUCKET_OFFSETS[*idx], 0))
            .map(|()| "ok".to_string()),
        PlanOp::RunOnce => engine.run_discard_worker_once().map(|()| "ok".to_string()),
    };
    match result {
        Ok(encoded) => encoded,
        Err(EngineError::Transaction(code)) => format!("err{code}"),
        Err(other) => format!("err{other:?}"),
    }
}

/// T0203 崩溃点：并发组合模式子进程（由环境变量分派，T0201 模式）。
#[test]
fn concurrent_combined_crash_child() {
    let Ok(engine_path) = std::env::var("SUBVOL_CC_ENGINE") else {
        return;
    };
    let plan_path = std::env::var("SUBVOL_CC_PLAN").unwrap();
    let log_path = std::env::var("SUBVOL_CC_LOG").unwrap();
    let ready = std::env::var("SUBVOL_CC_READY").unwrap();

    let plan_text = std::fs::read_to_string(&plan_path).unwrap();
    let mut lines: Vec<String> = Vec::new();
    let mut writers: Vec<Vec<PlanOp>> = (0..WRITERS).map(|_| Vec::new()).collect();
    for line in plan_text.lines() {
        if line.is_empty() {
            continue;
        }
        let mut it = line.splitn(2, ' ');
        let wid: usize = it.next().unwrap().parse().unwrap();
        writers[wid].push(decode_op(it.next().unwrap()));
    }

    let engine = std::sync::Arc::new(StorageEngine::create_persistent(&engine_path).unwrap());
    for offset in BUCKET_OFFSETS {
        engine.add_free_bucket(offset);
    }
    engine
        .inject_fault(FaultPoint::TransactionRestart, 6)
        .unwrap();

    /* 提交日志：Barrier 起跑后锁内执行 op 并追加，日志顺序 == 引擎提交顺序。 */
    let start = std::sync::Arc::new(std::sync::Barrier::new(WRITERS));
    let log: std::sync::Arc<Mutex<Vec<String>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
    let mut workers = Vec::new();
    for ops in writers {
        let engine = std::sync::Arc::clone(&engine);
        let log = std::sync::Arc::clone(&log);
        let start = std::sync::Arc::clone(&start);
        workers.push(std::thread::spawn(move || {
            start.wait();
            for op in ops {
                let mut guard = log.lock().unwrap();
                let encoded = execute_op(&engine, &op);
                guard.push(format!("{} | {}", encode_op(&op), encoded));
            }
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }

    /* 崩溃点：日志落盘 → journal 落盘（全部已提交 op durable）→ abort。 */
    let entries = log.lock().unwrap();
    let mut file = std::fs::File::create(&log_path).unwrap();
    for entry in entries.iter() {
        writeln!(file, "{entry}").unwrap();
    }
    file.sync_all().unwrap();
    drop(entries);
    engine.sync().unwrap();
    std::fs::write(ready, b"durable-before-abort").unwrap();
    std::process::abort();
}

/// T0202 组合域桶模型：与引擎 alloc 树/内存队列严格同构的影子状态。
/// state: 0=free、1=btree-owned、2=need-discard（对齐 T0197/T0202 模型）。
#[derive(Clone, Debug, Default)]
struct CombinedModel {
    btree: BTreeMap<KeyPosition, Vec<u64>>,
    state: [u8; N_BUCKETS],
    queued: [bool; N_BUCKETS],
    queue: VecDeque<usize>,
}

fn buckets() -> [KeyPosition; N_BUCKETS] {
    BUCKET_OFFSETS.map(|o| KeyPosition::new(0, o, 0))
}

/// 按提交日志重放模型：每个日志条目携带引擎实际结果，模型转换确定性。
/// 顺序 == 日志顺序 == 引擎提交顺序（测试锁保证）。
fn replay(log_text: &str, model: &mut CombinedModel) {
    for line in log_text.lines() {
        if line.is_empty() {
            continue;
        }
        let (op_part, result_part) = line.split_once(" | ").unwrap();
        let op = decode_op(op_part);
        match op {
            PlanOp::Put(i, o, s, v) => {
                if result_part == "ok" {
                    model.btree.insert(KeyPosition::new(i, o, s), v);
                }
            }
            PlanOp::Delete(i, o, s) => {
                if result_part == "ok" {
                    model.btree.remove(&KeyPosition::new(i, o, s));
                }
            }
            PlanOp::Allocate => {
                if let Some(rest) = result_part.strip_prefix("ok ") {
                    let mut it = rest.split_whitespace();
                    let _inode: u64 = it.next().unwrap().parse().unwrap();
                    let offset: u64 = it.next().unwrap().parse().unwrap();
                    let idx = (offset - BUCKET_OFFSETS[0]) as usize;
                    assert!(idx < N_BUCKETS, "allocate 必须落在模型桶域");
                    model.state[idx] = 1;
                } else {
                    assert!(
                        result_part.starts_with("err"),
                        "allocate 失败必须 err：{result_part}"
                    );
                }
            }
            PlanOp::Reclaim(idx) => {
                if result_part == "ok" {
                    /* T0202 组合域实证：reclaim 恒成功且 0↔2 toggle。 */
                    model.state[idx] = if model.state[idx] == 2 { 0 } else { 2 };
                }
            }
            PlanOp::Queue(idx) => {
                if result_part == "ok" {
                    assert!(!model.queued[idx], "queue 成功前提是未入队");
                    model.queued[idx] = true;
                    model.queue.push_back(idx);
                } else {
                    assert_eq!(result_part, "err-17", "重复 queue 必须 -17");
                }
            }
            PlanOp::RunOnce => {
                /* 引擎与模型队列同构（同 push/pop 序列）：队首由模型
                 * 队列决定（与引擎队列同步），结果由模型 state 判定。 */
                match model.queue.pop_front() {
                    None => {
                        assert_eq!(result_part, "err-11", "空队必须 -11");
                    }
                    Some(head) => {
                        if model.state[head] == 2 {
                            assert_eq!(result_part, "ok", "need-discard 队首必须成功");
                            model.queued[head] = false;
                            model.state[head] = 0;
                        } else {
                            assert_eq!(result_part, "err-11", "非 need-discard 必须回旋 -11");
                            model.queue.push_back(head);
                        }
                    }
                }
            }
        }
    }
}

fn rebuild_bucket_state(engine: &StorageEngine) -> [u8; N_BUCKETS] {
    let alloc_keys = engine.scan(BtreeId::new(4).unwrap()).unwrap();
    let mut state = [0u8; N_BUCKETS];
    for key in &alloc_keys {
        let pos = key.position();
        if pos.inode != 0 || !BUCKET_OFFSETS.contains(&pos.offset) {
            continue;
        }
        let idx = (pos.offset - BUCKET_OFFSETS[0]) as usize;
        /* bch_alloc_v4 data_type 位于 value[1] 字节 6（T0202 布局）：
         * 0=free、9=need-discard、其余=btree-owned。 */
        let data_type = ((key.value()[1] >> 48) & 0xff) as u8;
        state[idx] = match data_type {
            0 => 0, /* BCH_DATA_FREE */
            9 => 2, /* BCH_DATA_NEED_DISCARD（树位持久，崩溃可见） */
            _ => 1, /* btree-owned */
        };
    }
    state
}

/// T0203 父测试：生成并发组合计划 → 子进程崩溃 → 提交日志重放模型 →
/// 恢复后精确断言（btree + alloc 三态 + 队列语义）。
proptest! {
    #![proptest_config(ProptestConfig {
        cases: CASES,
        max_shrink_iters: 0,
        ..ProptestConfig::default()
    })]

    #[test]
    fn concurrent_combined_crash_recovery_exact(
        plan in plan_strategy(),
    ) {
    let engine_path = unique_tmp_path("engine");
    let plan_path = unique_tmp_path("plan");
    let log_path = unique_tmp_path("log");
    let ready_path = unique_tmp_path("ready");
    let _ = std::fs::remove_file(&engine_path);
    let _ = std::fs::remove_file(&plan_path);
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_file(&ready_path);

    let mut plan_text = String::new();
    for (wid, ops) in plan.iter().enumerate() {
        for op in ops {
            plan_text.push_str(&format!("{wid} {}\n", encode_op(op)));
        }
    }
    std::fs::write(&plan_path, plan_text).unwrap();

    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "concurrent_combined_crash_child",
            "--nocapture",
        ])
        .env("SUBVOL_CC_ENGINE", &engine_path)
        .env("SUBVOL_CC_PLAN", &plan_path)
        .env("SUBVOL_CC_LOG", &log_path)
        .env("SUBVOL_CC_READY", &ready_path)
        .status()
        .unwrap();
    assert!(!status.success(), "子进程必须崩溃 abort");

    let log_text = std::fs::read_to_string(&log_path).unwrap();
    let mut model = CombinedModel::default();
    replay(&log_text, &mut model);

    let recovered = StorageEngine::open_persistent(&engine_path).unwrap();

    /* btree 内容精确相等。 */
    let mut scanned: BTreeMap<KeyPosition, Vec<u64>> = BTreeMap::new();
    for key in recovered.scan(BtreeId::DEFAULT).unwrap() {
        scanned.insert(key.position(), key.value().to_vec());
    }
    prop_assert_eq!(scanned, model.btree, "并发崩溃后 btree 必须精确");

    /* alloc 三态精确：need_discard 树位持久（T0202 语义）。 */
    let rebuilt = rebuild_bucket_state(&recovered);
    prop_assert_eq!(rebuilt, model.state, "并发崩溃后桶状态必须精确");

    /* 队列语义：open_persistent 不自动恢复 fast_discard（darray 内存态），
     * discover 计数 == 模型 need-discard 桶数（树位持久）。 */
    prop_assert!(
        recovered.discard_queue_empty().unwrap(),
        "open_persistent 不得自动入队"
    );
    let discovered = recovered.discover_discard_buckets().unwrap();
    prop_assert_eq!(
        discovered,
        model.state.iter().filter(|&&s| s == 2).count(),
        "need_discard 树位计数必须等于模型"
    );

    recovered.verify_all().unwrap();
    drop(recovered);
    let _ = std::fs::remove_file(&engine_path);
    let _ = std::fs::remove_file(&plan_path);
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_file(&ready_path);
    }
}
