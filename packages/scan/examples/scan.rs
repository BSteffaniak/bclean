//! Scans a path and prints totals.
//!
//! ```text
//! cargo run --release -p bclean_scan --example scan -- [--portable] [--threads N]
//!     [--cross-fs] [--summarize NAME[,NAME...]] [--exclude GLOB]... <path>
//! ```
//!
//! `--summarize` installs a simple oracle that summarizes every directory with one of the given
//! names (e.g. `node_modules,target`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bclean_scan::{
    ListedEntry, NoPrune, PruneOracle, ScanBackend, ScanOptions, ScanProgress, scan,
};
use bclean_tree::{EntryKind, NodeId};

struct ByName(Vec<OsString>);

impl PruneOracle for ByName {
    fn summarize(&self, _dir: &Path, entries: &[ListedEntry<'_>]) -> Vec<usize> {
        entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == EntryKind::Dir && self.0.iter().any(|n| n == e.name))
            .map(|(i, _)| i)
            .collect()
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let mut root: Option<PathBuf> = None;
    let mut options = ScanOptions::new(".");
    let mut summarize: Vec<OsString> = Vec::new();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--portable") => options.backend = ScanBackend::Portable,
            Some("--cross-fs") => options.one_filesystem = false,
            Some("--threads") => {
                options.threads = args.next().and_then(|v| v.to_str()?.parse().ok());
            }
            Some("--summarize") => {
                if let Some(v) = args.next() {
                    summarize.extend(v.to_string_lossy().split(',').map(OsString::from));
                }
            }
            Some("--exclude") => {
                if let Some(v) = args.next() {
                    options.exclude.push(v.to_string_lossy().into_owned());
                }
            }
            _ => root = Some(PathBuf::from(arg)),
        }
    }
    let Some(root) = root else {
        eprintln!("usage: scan [--portable] [--threads N] [--cross-fs] [--summarize NAMES] <path>");
        return ExitCode::FAILURE;
    };
    options.root = root;

    let progress = ScanProgress::new();
    let by_name = ByName(summarize);
    let oracle: &dyn PruneOracle = if by_name.0.is_empty() {
        &NoPrune
    } else {
        &by_name
    };
    match scan(&options, oracle, &progress) {
        Ok(outcome) => {
            let info = outcome.tree.info(NodeId::ROOT);
            println!(
                "{}\n  backend={:?} threads={} elapsed={:.3}s\n  items={} nodes={} dirs={} files={}\n  apparent={} allocated={} ({:.2} GiB)\n  issues={}",
                outcome.tree.root_path().display(),
                outcome.backend,
                options.effective_threads(),
                outcome.elapsed.as_secs_f64(),
                info.items,
                outcome.tree.len(),
                outcome.stats.dirs,
                outcome.stats.files,
                info.total_apparent,
                info.total_allocated,
                to_gib(info.total_allocated),
                outcome.issues.len(),
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("scan failed: {e}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::cast_precision_loss)] // display only
fn to_gib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1u32 << 30)
}
