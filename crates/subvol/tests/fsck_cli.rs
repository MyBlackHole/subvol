use std::fs;
use std::path::PathBuf;
use std::process::Command;

use subvol::{BtreeId, BtreeKey, KeyPosition, StorageEngine};

fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "subvol-fsck-cli-{name}-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("t")
    ));
    path
}

fn run_fsck(path: &PathBuf) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_subvol-fsck"))
        .arg(path)
        .output()
        .unwrap()
}

#[test]
fn fsck_cli_healthy_image_exits_zero_and_prints_ok() {
    let path = temp_path("healthy");
    let engine = StorageEngine::create_persistent(&path).unwrap();
    engine
        .put(
            BtreeId::DEFAULT,
            BtreeKey::new(KeyPosition::new(0, 1, 0), vec![1]).unwrap(),
        )
        .unwrap();
    drop(engine);

    let output = run_fsck(&path);
    assert!(
        output.status.success(),
        "healthy image must pass: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "OK\n",
        "healthy image must print OK"
    );
    fs::remove_file(path).unwrap();
}

#[test]
fn fsck_cli_corrupt_image_exits_nonzero_and_prints_error_name() {
    let path = temp_path("corrupt");
    fs::write(&path, b"not a journal device").unwrap();

    let output = run_fsck(&path);
    assert!(!output.status.success(), "corrupt image must fail");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot open"),
        "corrupt image must print an error name: {stderr}"
    );
    fs::remove_file(path).unwrap();
}

#[test]
fn fsck_cli_missing_image_exits_two() {
    let path = temp_path("missing");
    let output = run_fsck(&path);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot open"));
}

fn run_fsck_args(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_subvol-fsck"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn fsck_cli_yes_mode_healthy_image_exits_zero_with_repair_output() {
    let path = temp_path("healthy-y");
    let engine = StorageEngine::create_persistent(&path).unwrap();
    engine
        .put(
            BtreeId::DEFAULT,
            BtreeKey::new(KeyPosition::new(0, 1, 0), vec![1]).unwrap(),
        )
        .unwrap();
    drop(engine);

    let output = run_fsck_args(&["-y", path.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "-y on a healthy image must pass: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "OK (repaired)\n",
        "-y mode must report the repair run"
    );
    fs::remove_file(path).unwrap();
}

#[test]
fn fsck_cli_no_repair_and_yes_are_mutually_exclusive() {
    let path = temp_path("ny-conflict");
    fs::write(&path, b"junk").unwrap();
    let output = run_fsck_args(&["-n", "-y", path.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("mutually exclusive"),
        "stderr must explain the conflict"
    );
    fs::remove_file(path).unwrap();
}
