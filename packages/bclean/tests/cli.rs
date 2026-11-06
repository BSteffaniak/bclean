//! End-to-end tests of the built `bclean` binary against a temp HOME and fixture tree.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
}

fn write(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![7_u8; bytes]).unwrap();
}

fn walk(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_path_buf()];
    if path.is_dir() {
        for e in std::fs::read_dir(path).unwrap() {
            out.extend(walk(&e.unwrap().path()));
        }
    }
    out
}

fn fixture(extra_config: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let home = base.join("home");
    let root = base.join("projects");
    write(&root.join("app/Cargo.toml"), 20);
    write(&root.join("app/src/main.rs"), 20);
    write(&root.join("app/target/debug/app"), 200_000);
    write(&root.join("web/package.json"), 2);
    write(&root.join("web/node_modules/left-pad/index.js"), 50_000);
    write(&root.join("old/Cargo.toml"), 20);
    write(&root.join("old/target/release/big"), 300_000);
    let old =
        filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(400 * 86_400));
    for p in walk(&root.join("old")) {
        filetime::set_symlink_file_times(&p, old, old).unwrap();
    }
    let config = home.join(".config/bclean/config.toml");
    write(&config, 0);
    std::fs::write(
        &config,
        format!(
            "[monitor]\nindex_roots = []\nvolumes = [\"{}\"]\n{extra_config}",
            home.display()
        ),
    )
    .unwrap();
    Fixture {
        _dir: dir,
        home,
        root,
    }
}

fn bclean(fx: &Fixture, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bclean"))
        .args(args)
        .current_dir(&fx.root)
        .env("HOME", &fx.home)
        .env("XDG_CONFIG_HOME", fx.home.join(".config"))
        .env("XDG_DATA_HOME", fx.home.join(".local/share"))
        .env("XDG_RUNTIME_DIR", fx.home.join("run"))
        .env("APPDATA", fx.home.join("AppData/Roaming"))
        .env("LOCALAPPDATA", fx.home.join("AppData/Local"))
        .env_remove("BCLEAN_CONFIG")
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_str(&ok(out)).unwrap()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn no_daemon_scan_ls_explain_clean() {
    let fx = fixture("");
    let root = s(&fx.root);

    let scan = json(&bclean(
        &fx,
        &["--no-daemon", "scan", root, "--format", "json"],
    ));
    assert_eq!(scan["reclaimable"]["ephemeral"]["items"], 3);
    assert!(scan["total_allocated"].as_u64().unwrap() > 0);
    let table = ok(&bclean(&fx, &["--no-daemon", "scan", root]));
    assert!(table.contains("reclaimable:"), "{table}");

    let ls = json(&bclean(
        &fx,
        &["--no-daemon", "ls", root, "--format", "json"],
    ));
    let paths: Vec<&str> = ls
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths.len(), 3, "{paths:?}");
    let stale = json(&bclean(
        &fx,
        &["--no-daemon", "ls", root, "--stale", "--format", "json"],
    ));
    assert_eq!(stale.as_array().unwrap().len(), 1);
    assert!(
        stale[0]["path"].as_str().unwrap().ends_with("old/target"),
        "{stale}"
    );
    let table = ok(&bclean(
        &fx,
        &["--no-daemon", "ls", root, "-n", "2", "--sort", "size"],
    ));
    assert!(table.contains("old/target"), "{table}");

    let explain = json(&bclean(
        &fx,
        &[
            "--no-daemon",
            "explain",
            s(&fx.root.join("app/target")),
            "--format",
            "json",
        ],
    ));
    assert_eq!(explain["classification"]["rule_id"], "cargo_target");

    // Dry run never acts.
    let dry = json(&bclean(
        &fx,
        &[
            "--no-daemon",
            "clean",
            root,
            "--dry-run",
            "--format",
            "json",
        ],
    ));
    assert_eq!(dry["report"]["dry_run"], true);
    assert!(fx.root.join("web/node_modules").exists());
    assert!(fx.root.join("app/target").exists());

    // Without --yes and without a terminal, cleaning is refused.
    let refused = bclean(
        &fx,
        &["--no-daemon", "clean", root, "--rule", "node_modules"],
    );
    assert_eq!(refused.status.code(), Some(1));
    assert!(fx.root.join("web/node_modules").exists());

    // Real clean of one rule.
    let done = json(&bclean(
        &fx,
        &[
            "--no-daemon",
            "clean",
            root,
            "--rule",
            "node_modules",
            "--yes",
            "--format",
            "json",
        ],
    ));
    assert_eq!(
        done["report"]["results"][0]["outcome"]["kind"], "DONE",
        "{done}"
    );
    assert!(!fx.root.join("web/node_modules").exists());
    assert!(fx.root.join("app/target").exists());

    let journal = json(&bclean(
        &fx,
        &["--no-daemon", "journal", "--format", "json"],
    ));
    assert_eq!(journal.as_array().unwrap().len(), 1);

    let rules = ok(&bclean(&fx, &["rules", "list"]));
    assert!(rules.contains("cargo_target"));
    let test = ok(&bclean(
        &fx,
        &["rules", "test", s(&fx.root.join("app/target"))],
    ));
    assert!(test.contains("cargo_target"), "{test}");
    let paths = ok(&bclean(&fx, &["config", "paths"]));
    assert!(paths.contains("daemon socket"));
}

#[test]
fn usage_errors_exit_2() {
    let fx = fixture("");
    assert_eq!(bclean(&fx, &["--bogus-flag"]).status.code(), Some(2));
    assert_eq!(
        bclean(&fx, &["ls", "--sort", "nope"]).status.code(),
        Some(2)
    );
}

#[cfg(unix)]
#[test]
fn daemon_autostart_status_stop() {
    let sock_dir = PathBuf::from(format!("/tmp/bclean-cli-{}", std::process::id()));
    let socket = sock_dir.join("d.sock");
    let fx = fixture(&format!("[daemon]\nsocket = \"{}\"\n", socket.display()));
    let root = s(&fx.root);

    let status = bclean(&fx, &["daemon", "status"]);
    assert_eq!(status.status.code(), Some(1), "daemon not running yet");

    // Any command autostarts the daemon.
    let ls = json(&bclean(&fx, &["ls", root, "--format", "json"]));
    assert_eq!(ls.as_array().unwrap().len(), 3);
    let status = json(&bclean(&fx, &["status", "--format", "json"]));
    assert!(status["daemon"]["pid"].as_u64().unwrap() > 0);
    assert_eq!(status["daemon"]["roots"][0]["root"], root);
    assert!(ok(&bclean(&fx, &["daemon", "status"])).contains("running"));

    // Dry-run clean through the daemon.
    let dry = json(&bclean(
        &fx,
        &["clean", root, "--stale", "--dry-run", "--format", "json"],
    ));
    assert_eq!(dry["plan"]["items"].as_array().unwrap().len(), 1);
    assert!(fx.root.join("old/target").exists());

    assert!(ok(&bclean(&fx, &["daemon", "stop"])).contains("stopped"));
    assert_eq!(bclean(&fx, &["daemon", "status"]).status.code(), Some(1));
    let log = ok(&bclean(&fx, &["daemon", "logs", "--tail", "50"]));
    assert!(log.contains("listening"), "{log}");
    let _ = std::fs::remove_dir_all(sock_dir);
}
