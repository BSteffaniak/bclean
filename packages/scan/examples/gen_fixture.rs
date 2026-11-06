//! Generates a deterministic directory tree for scanner benchmarks.
//!
//! ```text
//! cargo run --release -p bclean_scan --example gen_fixture -- <out-dir> [fanout] [depth] [files-per-leaf]
//! ```
//!
//! Defaults (`fanout = 6`, `depth = 4`, `files = 150`) create 1,296 leaf directories and
//! ~200k files. Every tenth file has a few KiB of content; the rest are empty, so the fixture
//! stays small on disk while exercising per-entry metadata costs.

use std::fs;
use std::io;
use std::path::Path;
use std::process::ExitCode;

fn build(
    dir: &Path,
    fanout: usize,
    depth: usize,
    files: usize,
    counter: &mut usize,
) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    if depth == 0 {
        for i in 0..files {
            *counter += 1;
            let len = if counter.is_multiple_of(10) {
                (*counter * 7919) % 8192
            } else {
                0
            };
            fs::write(dir.join(format!("file_{i:04}.dat")), vec![0x5a; len])?;
        }
        return Ok(());
    }
    // A couple of files at every level too.
    for i in 0..2 {
        *counter += 1;
        fs::write(dir.join(format!("meta_{i}.txt")), b"fixture")?;
    }
    for i in 0..fanout {
        build(
            &dir.join(format!("d{depth}_{i:02}")),
            fanout,
            depth - 1,
            files,
            counter,
        )?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(out) = args.first() else {
        eprintln!("usage: gen_fixture <out-dir> [fanout] [depth] [files-per-leaf]");
        return ExitCode::FAILURE;
    };
    let num =
        |i: usize, default: usize| args.get(i).and_then(|v| v.parse().ok()).unwrap_or(default);
    let (fanout, depth, files) = (num(1, 6), num(2, 4), num(3, 150));
    let mut counter = 0;
    match build(Path::new(out), fanout, depth, files, &mut counter) {
        Ok(()) => {
            println!("created {counter} files under {out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("gen_fixture failed: {e}");
            ExitCode::FAILURE
        }
    }
}
