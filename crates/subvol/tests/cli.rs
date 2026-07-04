use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::Command;
use std::time::Duration;

use tempfile::TempDir;

// ─── Helper ───

fn binary_in_same_dir(name: &str) -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test binary path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let bin = path.join(name);
    if bin.exists() {
        return bin;
    }
    let alt = path.parent().unwrap_or(&path).join(name);
    assert!(
        alt.exists(),
        "{name} binary not found at {:?} or {:?}",
        bin,
        alt
    );
    alt
}

struct TestContext {
    _dir: TempDir,
}

fn setup() -> TestContext {
    let dir = TempDir::new().expect("tempdir");
    TestContext { _dir: dir }
}

// ─── Tests ───

#[test]
fn test_cli_fuse_mount_read_write() {
    assert!(std::path::Path::new("/dev/fuse").exists(), "/dev/fuse is required");

    let ctx = setup();
    let home = ctx._dir.path();

    let mountpoint = home.join("fuse-mnt");
    fs::create_dir(&mountpoint).unwrap();
    let mountpoint_arg = mountpoint.to_str().unwrap();
    let cli_bin = binary_in_same_dir("subvol");
    let mut fuse = Command::new(cli_bin)
        .args(["fuse", mountpoint_arg, "-f"])
        .env("HOME", home)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start direct FUSE mount");

    let mounted = (0..100).any(|_| {
        let mounts = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        if mounts.contains("fuse.subvol") && mounts.contains(mountpoint_arg) {
            true
        } else {
            std::thread::sleep(Duration::from_millis(100));
            false
        }
    });
    if !mounted {
        let _ = fuse.kill();
        let output = fuse.wait_with_output().unwrap();
        panic!(
            "FUSE mount was not established: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let volume_path = mountpoint.join("1");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&volume_path)
        .unwrap();
    file.seek(SeekFrom::Start(7)).unwrap();
    if let Err(e) = file.write_all(b"fuse-io") {
        fuse.kill().ok();
        let output = fuse.wait_with_output().unwrap();
        eprintln!("FUSE stderr on write failure:\n{}", String::from_utf8_lossy(&output.stderr));
        drop(output);
        panic!("write failed: {e}");
    }
    file.flush().unwrap();
    file.seek(SeekFrom::Start(7)).unwrap();
    let mut data = [0u8; 7];
    file.read_exact(&mut data).unwrap();
    assert_eq!(&data, b"fuse-io");

    file.seek(SeekFrom::Start(4096)).unwrap();
    if let Err(e) = file.write_all(&[0x5a; 4096]) {
        fuse.kill().ok();
        let output = fuse.wait_with_output().unwrap();
        eprintln!("FUSE stderr on second write failure:\n{}", String::from_utf8_lossy(&output.stderr));
        drop(output);
        panic!("second write failed: {e}");
    }
    if let Err(e) = file.sync_all() {
        fuse.kill().ok();
        let output = fuse.wait_with_output().unwrap();
        eprintln!("FUSE stderr on sync failure:\n{}", String::from_utf8_lossy(&output.stderr));
        drop(output);
        panic!("sync_all failed: {e}");
    }
    drop(file);

    let unmount = Command::new("fusermount")
        .args(["-u", mountpoint_arg])
        .output()
        .expect("failed to run fusermount");
    assert!(
        unmount.status.success(),
        "fusermount failed: {}",
        String::from_utf8_lossy(&unmount.stderr)
    );

    let exited = (0..100).any(|_| match fuse.try_wait() {
        Ok(Some(status)) => {
            if !status.success() {
                let mut stderr = String::new();
                if let Some(mut pipe) = fuse.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                panic!("FUSE process failed with {status}: {stderr}");
            }
            true
        }
        Ok(None) => {
            std::thread::sleep(Duration::from_millis(100));
            false
        }
        Err(error) => panic!("failed waiting for FUSE process: {error}"),
    });
    if !exited {
        let _ = fuse.kill();
        let _ = fuse.wait();
        panic!("FUSE process did not exit after unmount");
    }
}

#[test]
#[ignore = "fuser 0.17.0 does not support FUSE_FALLOCATE init flag; mkfs.xfs requires fallocate punch-hole"]
fn test_cli_fuse_mount_xfs_file_operations() {
    assert!(std::path::Path::new("/dev/fuse").exists(), "/dev/fuse is required");
    if Command::new("mkfs.xfs").arg("-V").output().is_err() {
        eprintln!("skipping XFS integration test: mkfs.xfs is unavailable");
        return;
    }

    let ctx = setup();
    let home = ctx._dir.path();

    let outer_mountpoint = home.join("fuse-xfs-mnt");
    let xfs_mountpoint = home.join("xfs-mnt");
    fs::create_dir(&outer_mountpoint).unwrap();
    fs::create_dir(&xfs_mountpoint).unwrap();
    let outer_arg = outer_mountpoint.to_str().unwrap();
    let xfs_arg = xfs_mountpoint.to_str().unwrap();
    let mut fuse = Command::new(binary_in_same_dir("subvol"))
        .args(["fuse", outer_arg, "-f"])
        .env("HOME", home)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start direct FUSE mount");

    let mounted = (0..100).any(|_| {
        let mounts = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        if mounts.contains("fuse.subvol") && mounts.contains(outer_arg) {
            true
        } else {
            std::thread::sleep(Duration::from_millis(100));
            false
        }
    });
    if !mounted {
        let _ = fuse.kill();
        let output = fuse.wait_with_output().unwrap();
        panic!(
            "FUSE mount was not established: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let image = outer_mountpoint.join("1");
    let mkfs = Command::new("mkfs.xfs")
        .args(["-f", image.to_str().unwrap()])
        .output()
        .expect("failed to run mkfs.xfs");
    assert!(
        mkfs.status.success(),
        "mkfs.xfs failed: stdout={} stderr={}",
        String::from_utf8_lossy(&mkfs.stdout),
        String::from_utf8_lossy(&mkfs.stderr)
    );

    let mount = Command::new("mount")
        .args(["-t", "xfs", "-o", "loop", image.to_str().unwrap(), xfs_arg])
        .output()
        .expect("failed to run mount");
    if !mount.status.success() {
        let error = String::from_utf8_lossy(&mount.stderr);
        let _ = Command::new("fusermount").args(["-u", outer_arg]).output();
        let _ = fuse.wait();
        let lower_error = error.to_ascii_lowercase();
        if lower_error.contains("permission denied")
            || lower_error.contains("operation not permitted")
            || lower_error.contains("权限不够")
        {
            eprintln!("skipping XFS integration test: mount permission denied: {error}");
            return;
        }
        panic!("mount -t xfs failed: {error}");
    }

    let dir = xfs_mountpoint.join("dir");
    fs::create_dir(&dir).unwrap();
    let old_file = dir.join("old");
    let new_file = dir.join("new");
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&old_file)
        .unwrap();
    file.write_all(b"xfs-through-fuse").unwrap();
    file.sync_all().unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut data = Vec::new();
    file.read_to_end(&mut data).unwrap();
    assert_eq!(data, b"xfs-through-fuse");
    drop(file);
    fs::rename(&old_file, &new_file).unwrap();
    fs::remove_file(&new_file).unwrap();
    fs::remove_dir(&dir).unwrap();

    let unmount_xfs = Command::new("umount")
        .arg(xfs_arg)
        .output()
        .expect("failed to unmount XFS");
    assert!(
        unmount_xfs.status.success(),
        "XFS unmount failed: {}",
        String::from_utf8_lossy(&unmount_xfs.stderr)
    );
    let unmount_fuse = Command::new("fusermount")
        .args(["-u", outer_arg])
        .output()
        .expect("failed to unmount FUSE");
    assert!(
        unmount_fuse.status.success(),
        "FUSE unmount failed: {}",
        String::from_utf8_lossy(&unmount_fuse.stderr)
    );
    let fuse_status = fuse.wait().unwrap();
    assert!(fuse_status.success(), "FUSE process failed: {fuse_status}");
}

#[test]
fn test_cli_fuse_mount_background_readiness() {
    assert!(std::path::Path::new("/dev/fuse").exists(), "/dev/fuse is required");

    let ctx = setup();
    let home = ctx._dir.path();

    let mountpoint = home.join("fuse-bg-mnt");
    fs::create_dir(&mountpoint).unwrap();
    let mountpoint_arg = mountpoint.to_str().unwrap();
    let mut mount = Command::new(binary_in_same_dir("subvol"))
        .args(["fuse", mountpoint_arg])
        .env("HOME", home)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to start background FUSE mount");
    let mount_status = mount.wait().unwrap();
    assert!(
        mount_status.success(),
        "background mount failed: {mount_status}"
    );

    let mounts = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    assert!(mounts.contains("fuse.subvol") && mounts.contains(mountpoint_arg));

    let unmount = Command::new("fusermount")
        .args(["-u", mountpoint_arg])
        .output()
        .expect("failed to run fusermount");
    assert!(
        unmount.status.success(),
        "fusermount failed: {}",
        String::from_utf8_lossy(&unmount.stderr)
    );

    let unmounted = (0..100).any(|_| {
        let mounts = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        if !mounts.contains("fuse.subvol") || !mounts.contains(mountpoint_arg) {
            true
        } else {
            std::thread::sleep(Duration::from_millis(100));
            false
        }
    });
    assert!(unmounted, "background FUSE mount remained after unmount");
}
