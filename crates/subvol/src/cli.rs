use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "subvol",
    version,
    about = "Copy-on-write block volume manager"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Mount the pool via FUSE
    Fuse {
        /// 挂载点路径
        mountpoint: String,
        /// 前台运行
        #[arg(short = 'f', long)]
        foreground: bool,
    },
    /// Start the NBD server (HTTP API + NBD export)
    Nbd {
        /// 前台运行（默认后台）
        #[arg(short = 'f', long)]
        foreground: bool,
    },
}

impl Cli {
    pub fn parse_args() -> Self {
        <Self as Parser>::parse()
    }
}
