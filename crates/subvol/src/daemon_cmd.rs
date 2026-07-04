use crate::config::SubvolmountdConfig;
use crate::daemon;

pub fn execute_nbd_start(
    config: &SubvolmountdConfig,
    config_path: &std::path::Path,
    foreground: bool,
) {
    let config_path = config_path.to_owned();
    let config = config.clone();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()
        .expect("failed to build NBD runtime");

    if foreground {
        println!("subvol NBD server started (foreground)");
        rt.block_on(daemon::run(config, config_path));
    } else {
        println!("subvol NBD server started (background)");
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .name("subvol-nbd".into())
            .spawn(move || {
                rt.block_on(daemon::run(config, config_path));
            })
            .expect("failed to spawn NBD thread");
    }
}
