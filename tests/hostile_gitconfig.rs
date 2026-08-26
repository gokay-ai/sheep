//! The shadow must not inherit the machine's `core.autocrlf`.
//!
//! This is a test binary of its own because it sets `GIT_CONFIG_GLOBAL` on the
//! process. `tests/collect_read_order.rs` is the same shape for `PATH`. A mutex
//! around the one test that writes the variable does not help: forty other
//! tests in `adversarial` spawn `git` without taking that lock, and a merge
//! that runs after the hostile file has been deleted aborts before it writes
//! conflict stages — which is how `refuses_a_worktree_with_unresolved_conflicts`
//! went red on a Linux runner while the same suite was green on a laptop.

use sheep::ops::{self, SnapMeta};
use sheep::repo::Worktree;
use sheep::store::TurnKind;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

const BUDGET: usize = 60_000;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(args)
        .output()
        .expect("git should run");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn line_endings_survive_a_hostile_global_gitconfig() {
    // The shadow reads repo-local config from its own bare git dir, never the
    // user's — but it inherited the *machine's* global config, where
    // `core.autocrlf = input` is routine advice on macOS and Linux. A CRLF file
    // was recorded as LF and written back as LF; the checkpoint was normalised
    // on the way in too, so the undo did not restore the original bytes either;
    // and `sheep snap` afterwards said "nothing changed since the last turn",
    // so Sheep could not see the damage it had done.
    let dir = TempDir::new().expect("tempdir");
    let base = std::fs::canonicalize(dir.path()).expect("canonicalize");
    let repo = base.join("repo");
    let state = base.join("state");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    git(&repo, &["init", "--quiet", "-b", "main"]);

    let hostile = base.join("hostile.gitconfig");
    std::fs::write(&hostile, "[core]\n\tautocrlf = input\n").unwrap();
    std::env::set_var("GIT_CONFIG_GLOBAL", &hostile);

    let crlf = b"line one\r\nline two\r\n";
    std::fs::write(repo.join("windows.bat"), crlf).unwrap();
    // Commit with the file's bytes as they are, so the repository itself is not
    // the thing that normalised them.
    git(&repo, &["-c", "core.autocrlf=false", "add", "-A"]);
    git(&repo, &["-c", "core.autocrlf=false", "commit", "--quiet", "-m", "base"]);

    let wt = Worktree::discover(&repo).expect("worktree");
    let intact = ops::snap(
        &wt,
        &state,
        "default",
        BUDGET,
        TurnKind::Turn,
        SnapMeta { note: Some("turn 1".into()), ..Default::default() },
        false,
    )
    .expect("snap should succeed")
    .expect("snap should record a turn")
    .seq;

    std::fs::write(repo.join("windows.bat"), b"CHANGED\r\n").unwrap();
    ops::snap(
        &wt,
        &state,
        "default",
        BUDGET,
        TurnKind::Turn,
        SnapMeta { note: Some("turn 2".into()), ..Default::default() },
        false,
    )
    .expect("second snap should succeed")
    .expect("second snap should record a turn");

    ops::restore(&wt, &state, "default", &intact.to_string(), BUDGET).unwrap();

    let after = std::fs::read(repo.join("windows.bat")).unwrap();
    assert_eq!(
        after, crlf,
        "the bytes written back must be the bytes recorded, whatever the machine's gitconfig says"
    );
}
