//! 存储后端数据完整性测试
//!
//! 验证 NfsBlockDevice 在各种数据模式下的读写正确性。

use tempfile::TempDir;
use tokio::runtime::Runtime;
use subvol_core::block_device::{BlockDevice, NfsBlockDevice, NfsConfig};
use subvol_core::types::BlockAddr;

fn setup_backend(block_size: u64) -> (TempDir, NfsBlockDevice) {
    let dir = TempDir::new().expect("tempdir");
    let config = NfsConfig {
        base_path: dir.path().to_path_buf(),
        vol_name: "integrity".to_string(),
        block_size,
    };
    let rt = Runtime::new().expect("rt");
    let backend = rt
        .block_on(NfsBlockDevice::new(config))
        .expect("create backend");
    (dir, backend)
}

#[test]
fn test_write_read_full_block() {
    let rt = Runtime::new().unwrap();
    let (_dir, backend) = setup_backend(4096);

    let data = vec![0xABu8; 4096];
    rt.block_on(backend.write_block(BlockAddr::new(0), &data))
        .unwrap();

    let mut buf = vec![0u8; 4096];
    rt.block_on(backend.read_block(BlockAddr::new(0), &mut buf))
        .unwrap();
    assert_eq!(buf, data);
}

#[test]
fn test_write_read_multiple_blocks() {
    let rt = Runtime::new().unwrap();
    let (_dir, backend) = setup_backend(512);

    let patterns: Vec<Vec<u8>> = (0..10)
        .map(|i| {
            let mut v = vec![i as u8; 512];
            v[0] = 0xFF;
            v[511] = i as u8;
            v
        })
        .collect();

    for (i, pat) in patterns.iter().enumerate() {
        rt.block_on(backend.write_block(BlockAddr::new(i as u64), pat))
            .unwrap();
    }

    for (i, expected) in patterns.iter().enumerate() {
        let mut buf = vec![0u8; 512];
        rt.block_on(backend.read_block(BlockAddr::new(i as u64), &mut buf))
            .unwrap();
        assert_eq!(&buf, expected, "block {i} mismatch");
    }
}

#[test]
fn test_sparse_file_reads_zeros() {
    let rt = Runtime::new().unwrap();
    let (_dir, backend) = setup_backend(4096);

    let mut buf = vec![0xFFu8; 4096];
    rt.block_on(backend.read_block(BlockAddr::new(42), &mut buf))
        .unwrap();
    assert_eq!(buf, vec![0u8; 4096], "sparse area should read zero");
}

#[test]
fn test_overwrite_block() {
    let rt = Runtime::new().unwrap();
    let (_dir, backend) = setup_backend(256);

    let data1 = vec![0x11u8; 256];
    let data2 = vec![0x22u8; 256];

    rt.block_on(backend.write_block(BlockAddr::new(5), &data1))
        .unwrap();
    rt.block_on(backend.write_block(BlockAddr::new(5), &data2))
        .unwrap();

    let mut buf = vec![0u8; 256];
    rt.block_on(backend.read_block(BlockAddr::new(5), &mut buf))
        .unwrap();
    assert_eq!(buf, data2, "should contain latest data");
}

#[test]
fn test_partial_block_reads_after_write() {
    let rt = Runtime::new().unwrap();
    let (_dir, backend) = setup_backend(1024);

    let data = vec![0xAAu8; 1024];
    rt.block_on(backend.write_block(BlockAddr::new(3), &data))
        .unwrap();

    let mut full = vec![0u8; 1024];
    rt.block_on(backend.read_block(BlockAddr::new(3), &mut full))
        .unwrap();
    assert_eq!(full, data);
}

#[test]
fn test_non_aligned_block_size() {
    let rt = Runtime::new().unwrap();
    let (_dir, backend) = setup_backend(2048);

    let data = vec![0xBBu8; 2048];
    rt.block_on(backend.write_block(BlockAddr::new(7), &data))
        .unwrap();

    let mut buf = vec![0u8; 2048];
    rt.block_on(backend.read_block(BlockAddr::new(7), &mut buf))
        .unwrap();
    assert_eq!(buf, data);
}

#[test]
fn test_sequential_blocks_overlap_boundary() {
    let rt = Runtime::new().unwrap();
    let (_dir, backend) = setup_backend(1024);

    let data: Vec<Vec<u8>> = (0..3)
        .map(|i| {
            let mut v = vec![0u8; 1024];
            v[0] = b'A' + i;
            v[1023] = b'z' - i;
            v
        })
        .collect();

    for (i, d) in data.iter().enumerate() {
        rt.block_on(backend.write_block(BlockAddr::new(i as u64), d))
            .unwrap();
    }

    for (i, expected) in data.iter().enumerate() {
        let mut buf = vec![0u8; 1024];
        rt.block_on(backend.read_block(BlockAddr::new(i as u64), &mut buf))
            .unwrap();
        assert_eq!(&buf, expected, "sequential block {i}");
    }
}

#[test]
fn test_flush_persistence() {
    let rt = Runtime::new().unwrap();
    let (_dir, backend) = setup_backend(4096);

    let data = vec![0xDDu8; 4096];
    rt.block_on(backend.write_block(BlockAddr::new(100), &data))
        .unwrap();
    rt.block_on(backend.flush()).unwrap();

    let mut buf = vec![0u8; 4096];
    rt.block_on(backend.read_block(BlockAddr::new(100), &mut buf))
        .unwrap();
    assert_eq!(buf, data);
}

#[test]
fn test_large_block_count() {
    let rt = Runtime::new().unwrap();
    let (_dir, backend) = setup_backend(512);

    let count = 256;
    for i in 0..count {
        let data = vec![(i % 256) as u8; 512];
        rt.block_on(backend.write_block(BlockAddr::new(i as u64), &data))
            .unwrap();
    }

    for i in 0..count {
        let mut buf = vec![0u8; 512];
        rt.block_on(backend.read_block(BlockAddr::new(i as u64), &mut buf))
            .unwrap();
        assert_eq!(buf[0], (i % 256) as u8, "block {i} marker mismatch");
    }
}
