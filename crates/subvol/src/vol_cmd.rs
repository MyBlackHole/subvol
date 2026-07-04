use std::io::Write;

pub fn execute_fuse_mount(
    config: &subvol_core::config::SubvolmountdConfig,
    mountpoint: &str,
    foreground: bool,
) {
    let mount_path = std::path::Path::new(mountpoint);
    if !mount_path.exists() {
        eprintln!("error: mountpoint '{mountpoint}' does not exist");
        std::process::exit(1);
    }

    if foreground {
        mount_fuse_foreground(config, mountpoint);
    } else {
        mount_fuse_background(config, mountpoint);
    }
}

fn mount_fuse_foreground(
    config: &subvol_core::config::SubvolmountdConfig,
    mountpoint: &str,
) {
    let pool_dir = config.pool_dir();
    let mp = std::path::Path::new(mountpoint).to_owned();

    // 对齐 bcachefs: 为整个 mount 会话创建一个 multi-thread runtime。
    // open_pool 在此 rt 上执行，start() → bch2_fs_read_write() 启动的 journal
    // background reclaim / auto flush 等 tokio::spawn 任务将附着在此 rt 上，
    // 与 FUSE I/O 操作共享同一 runtime，避免后台任务因 runtime 被 drop 而意外终止。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime for FUSE mount");

    let vol = rt.block_on(async {
        subvol_core::BchVol::open_pool(&pool_dir, "pool")
            .await
            .map_err(|e| format!("{e}"))
    });

    let result = match vol {
        Ok(vol) => {
            let fs = subvol_fuse::VolFuseFs::with_runtime(vol, rt, None);
            fs.mount(&mp).map_err(|e| format!("{e}"))
        }
        Err(error) => Err(error),
    };

    match result {
        Ok(()) => println!("FUSE volume unmounted"),
        Err(e) => {
            eprintln!("error: FUSE mount failed: {e}");
            std::process::exit(1);
        }
    }
}

fn mount_fuse_background(
    config: &subvol_core::config::SubvolmountdConfig,
    mountpoint: &str,
) {
    use std::os::unix::io::FromRawFd;

    let mut pipe_fds = [0i32; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        eprintln!("error: pipe() failed");
        std::process::exit(1);
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        eprintln!("error: fork() failed");
        let _ = unsafe { libc::close(pipe_fds[0]) };
        let _ = unsafe { libc::close(pipe_fds[1]) };
        std::process::exit(1);
    }

    if pid > 0 {
        let _ = unsafe { libc::close(pipe_fds[1]) };
        let mut buf = [0u8; 1];
        let mut read_file = unsafe { std::fs::File::from_raw_fd(pipe_fds[0]) };
        use std::io::Read;
        let n = read_file.read(&mut buf);
        match n {
            Ok(1) if buf[0] == 0 => {
                println!("Pool mounted at {mountpoint} (PID {pid})");
                println!("  Run 'fusermount -u {mountpoint}' to unmount");
            }
            _ => {
                let _ = unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
                eprintln!("error: FUSE mount failed (see /tmp/subvol-fuse.log)");
                std::process::exit(1);
            }
        }
    } else {
        let _ = unsafe { libc::close(pipe_fds[0]) };
        if unsafe { libc::setsid() } < 0 {
            eprintln!("error: setsid() failed");
            std::process::exit(1);
        }

        // 关闭 stdin，脱离终端
        if let Ok(null) = std::fs::File::open("/dev/null") {
            let null_fd = std::os::unix::io::IntoRawFd::into_raw_fd(null);
            if unsafe { libc::dup2(null_fd, libc::STDIN_FILENO) } < 0 {
                let _ = unsafe { libc::close(null_fd) };
            }
            let _ = unsafe { libc::close(null_fd) };
        }

        if let Ok(f) = std::fs::File::create("/tmp/subvol-fuse.log") {
            let fd = std::os::unix::io::IntoRawFd::into_raw_fd(f);
            // stdout + stderr 都重定向到日志文件
            if unsafe { libc::dup2(fd, libc::STDOUT_FILENO) } < 0
                || unsafe { libc::dup2(fd, libc::STDERR_FILENO) } < 0
            {
                let _ = unsafe { libc::close(fd) };
                eprintln!("error: dup2(stdout/stderr) failed");
                std::process::exit(1);
            }
            let _ = unsafe { libc::close(fd) };
        }

        let mut signal_write = unsafe { std::fs::File::from_raw_fd(pipe_fds[1]) };

        // 对齐 bcachefs fork 后: 创建专属 multi-thread runtime。
        // open_pool 在此 rt 上执行后，journal bg 任务将在此 rt 上长期运行。
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to create tokio runtime for FUSE mount");

        let pool_dir = config.pool_dir();
        let mp = std::path::Path::new(mountpoint).to_owned();

        let vol = rt.block_on(async {
            subvol_core::BchVol::open_pool(&pool_dir, "pool")
                .await
                .map_err(|e| format!("{e}"))
        });

        let result = match vol {
            Ok(vol) => match signal_write.try_clone() {
                Ok(signal_fd) => {
                    let fs = subvol_fuse::VolFuseFs::with_runtime(vol, rt, Some(signal_fd));
                    fs.mount(&mp).map_err(|e| format!("{e}"))
                }
                Err(error) => Err(format!("{error}")),
            },
            Err(error) => Err(error),
        };

        match result {
            Ok(()) => eprintln!("fuse: pool unmounted"),
            Err(e) => {
                let _ = signal_write.write_all(&[1]);
                eprintln!("fuse: mount failed: {e}");
                std::process::exit(1);
            }
        }
    }
}
