//! NfsBlockDevice 微基准
//!
//! 测量 read_block / write_block 在不同 block size 和访问模式下的延迟。
//!
//! 运行: cargo bench -p subvol-core
//! 查看: cargo bench -p subvol-core -- --verbose

use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use tempfile::TempDir;
use tokio::runtime::Runtime;
use subvol_core::block_device::{BlockDevice, NfsBlockDevice, NfsConfig};
use subvol_core::types::BlockAddr;

// ─── Helper ───

/// 创建临时 NfsBlockDevice，返回 (TempDir, backend)
fn create_backend(block_size: u64) -> (TempDir, NfsBlockDevice) {
    let dir = TempDir::new().expect("tempdir");
    let config = NfsConfig {
        base_path: dir.path().to_path_buf(),
        vol_name: "bench-vol".to_string(),
        block_size,
    };
    let rt = Runtime::new().expect("tokio rt");
    let backend = rt
        .block_on(NfsBlockDevice::new(config))
        .expect("NfsBlockDevice::new");
    (dir, backend)
}

/// 预写 blocks，用于读基准
fn prewrite(rt: &Runtime, backend: &NfsBlockDevice, block_size: u64, count: u64) {
    let data = vec![0xABu8; block_size as usize];
    for i in 0..count {
        rt.block_on(backend.write_block(BlockAddr::new(i), &data))
            .expect("prewrite");
    }
}

// ─── 写基准 ───

macro_rules! bench_write {
    ($name:ident, $block_size:expr) => {
        fn $name(c: &mut Criterion) {
            let rt = Runtime::new().expect("tokio rt");
            let (_dir, backend) = create_backend($block_size);
            let data = vec![0xABu8; $block_size as usize];
            let mut addr_counter = 0u64;

            let mut group = c.benchmark_group(format!("nfs_write_{}k", $block_size / 1024));
            group.measurement_time(Duration::from_secs(10));
            group.sample_size(100);

            group.bench_function("seq", |b| {
                b.iter_batched(
                    || {
                        let addr = BlockAddr::new(addr_counter);
                        addr_counter += 1;
                        addr
                    },
                    |addr| {
                        rt.block_on(backend.write_block(addr, &data))
                            .expect("write_block");
                    },
                    BatchSize::SmallInput,
                );
            });

            group.finish();
        }
    };
}

bench_write!(bench_write_4k, 4096);
bench_write!(bench_write_16k, 16384);
bench_write!(bench_write_64k, 65536);

// ─── 读基准 ───

macro_rules! bench_read {
    ($name:ident, $block_size:expr, $prewrite_count:expr) => {
        fn $name(c: &mut Criterion) {
            let rt = Runtime::new().expect("tokio rt");
            let (_dir, backend) = create_backend($block_size);
            prewrite(&rt, &backend, $block_size, $prewrite_count);

            let mut buf = vec![0u8; $block_size as usize];
            let mut idx = 0u64;

            let mut group = c.benchmark_group(format!("nfs_read_{}k", $block_size / 1024));
            group.measurement_time(Duration::from_secs(10));
            group.sample_size(100);

            group.bench_function("seq", |b| {
                b.iter(|| {
                    let addr = BlockAddr::new(idx % $prewrite_count);
                    idx += 1;
                    rt.block_on(backend.read_block(addr, &mut buf))
                        .expect("read_block");
                    black_box(&buf);
                });
            });

            group.finish();
        }
    };
}

bench_read!(bench_read_4k, 4096, 4096);
bench_read!(bench_read_16k, 16384, 1024);
bench_read!(bench_read_64k, 65536, 256);

// ─── 随机 4K 读（模拟 NBD 随机 IO） ───

fn bench_read_rand_4k(c: &mut Criterion) {
    const BLOCK_SIZE: u64 = 4096;
    const BLOCK_COUNT: u64 = 4096;
    const SEED: u64 = 42;

    let rt = Runtime::new().expect("tokio rt");
    let (_dir, backend) = create_backend(BLOCK_SIZE);
    prewrite(&rt, &backend, BLOCK_SIZE, BLOCK_COUNT);

    // 用 MMIX LCG 生成确定性的伪随机地址序列
    // 公式: x_{n+1} = x_n * 6364136223846793005 + 1442695040888963407
    let rand_addrs: Vec<BlockAddr> = {
        let mut x = SEED;
        let mut addrs = Vec::with_capacity(BLOCK_COUNT as usize);
        for _ in 0..BLOCK_COUNT {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            addrs.push(BlockAddr::new(x % BLOCK_COUNT));
        }
        addrs
    };

    let mut buf = vec![0u8; BLOCK_SIZE as usize];
    let mut idx = 0usize;

    let mut group = c.benchmark_group("nfs_read_4k_rand");
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(100);

    group.bench_function("rand", |b| {
        b.iter(|| {
            let addr = rand_addrs[idx % rand_addrs.len()];
            idx += 1;
            rt.block_on(backend.read_block(addr, &mut buf))
                .expect("read_block");
            black_box(&buf);
        });
    });

    group.finish();
}

// ─── 吞吐量基准（顺序读/写，MiB/s 视角） ───

fn bench_throughput_write_4k(c: &mut Criterion) {
    const BLOCK_SIZE: u64 = 4096;
    const BLOCKS_PER_ITER: u64 = 256;
    const N_WARMUP: u64 = 64;

    let rt = Runtime::new().expect("tokio rt");
    let (_dir, backend) = create_backend(BLOCK_SIZE);

    let warmup = vec![0u8; BLOCK_SIZE as usize];
    for i in 0..N_WARMUP {
        rt.block_on(backend.write_block(BlockAddr::new(i), &warmup))
            .expect("warmup");
    }

    let data = vec![0xABu8; BLOCK_SIZE as usize];
    let mut next_addr = N_WARMUP;

    let mut group = c.benchmark_group("nfs_throughput_write_4k");
    group.throughput(criterion::Throughput::Bytes(BLOCK_SIZE * BLOCKS_PER_ITER));
    group.measurement_time(Duration::from_secs(15));
    group.sample_size(50);

    group.bench_function("seq_1mib", |b| {
        b.iter_batched(
            || {
                let start = next_addr;
                next_addr += BLOCKS_PER_ITER;
                start
            },
            |start| {
                for i in 0..BLOCKS_PER_ITER {
                    rt.block_on(backend.write_block(BlockAddr::new(start + i), &data))
                        .expect("write_block");
                }
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_throughput_read_4k(c: &mut Criterion) {
    const BLOCK_SIZE: u64 = 4096;
    const BLOCKS_PER_ITER: u64 = 256;
    const TOTAL_BLOCKS: u64 = 2048;

    let rt = Runtime::new().expect("tokio rt");
    let (_dir, backend) = create_backend(BLOCK_SIZE);
    prewrite(&rt, &backend, BLOCK_SIZE, TOTAL_BLOCKS);

    let mut buf = vec![0u8; BLOCK_SIZE as usize];
    let mut start_idx = 0u64;

    let mut group = c.benchmark_group("nfs_throughput_read_4k");
    group.throughput(criterion::Throughput::Bytes(BLOCK_SIZE * BLOCKS_PER_ITER));
    group.measurement_time(Duration::from_secs(15));
    group.sample_size(50);

    group.bench_function("seq_1mib", |b| {
        b.iter_batched(
            || {
                let s = start_idx;
                start_idx = (start_idx + BLOCKS_PER_ITER) % TOTAL_BLOCKS;
                s
            },
            |start| {
                for i in 0..BLOCKS_PER_ITER {
                    let addr = BlockAddr::new((start + i) % TOTAL_BLOCKS);
                    rt.block_on(backend.read_block(addr, &mut buf))
                        .expect("read_block");
                }
                black_box(&buf);
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ─── 注册 ───

criterion_group!(
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(3))
        .significance_level(0.05);
    targets =
        bench_write_4k,
        bench_write_16k,
        bench_write_64k,
        bench_read_4k,
        bench_read_16k,
        bench_read_64k,
        bench_read_rand_4k,
        bench_throughput_write_4k,
        bench_throughput_read_4k,
);
criterion_main!(benches);
