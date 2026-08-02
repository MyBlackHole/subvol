//! subvol-fsck: engine-local consistency check CLI.
//!
//! Mirrors the bcachefs fsck command flow (src/commands/fsck.rs:419-447):
//! open the device/file, run every consistency pass, print errors, and
//! exit with a status code.  The engine has no repair path, so this CLI
//! is the no-repair mode only, matching upstream `-n/--no_repair`
//! ("Don't repair, only check for errors", fsck.rs:60-61).
//!
//! Exit codes (extending the fsck errcode channel to distinguish failure
//! classes): 0 = check passed, 1 = consistency check failed (verify_all
//! error, e.g. a DerivedStateMismatch variant), 2 = open/IO error.

use std::env;
use std::process;

use subvol::{fsck_image, EngineError};

const USAGE: &str = "usage: subvol-fsck [-n] [-f] <image-file>";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut no_repair = false;
    let mut force = false;
    let mut path = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-n" | "--no-repair" => no_repair = true,
            "-f" | "--force" => force = true,
            "-h" | "--help" => {
                println!("{USAGE}\n  -n, --no-repair  only check, never repair (the only mode: the engine has no repair path)\n  -f, --force      check even if the filesystem is marked clean (accepted for fsck parity)");
                process::exit(0);
            }
            _ if arg.starts_with('-') => {
                eprintln!("subvol-fsck: unknown option {arg}");
                eprintln!("{USAGE}");
                process::exit(2);
            }
            _ => {
                if path.is_some() {
                    eprintln!("subvol-fsck: too many arguments");
                    eprintln!("{USAGE}");
                    process::exit(2);
                }
                path = Some(arg.clone());
            }
        }
    }
    let _ = (no_repair, force);

    let path = match path {
        Some(path) => path,
        None => {
            eprintln!("subvol-fsck: missing image path");
            eprintln!("{USAGE}");
            process::exit(2);
        }
    };

    match fsck_image(&path) {
        Ok(()) => {
            println!("OK");
            process::exit(0);
        }
        Err(EngineError::Io(error)) => {
            eprintln!("subvol-fsck: cannot open {path}: {error}");
            process::exit(2);
        }
        Err(error) => {
            eprintln!("ERROR: {error}");
            process::exit(1);
        }
    }
}
