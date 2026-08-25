//! The order `ops::collect` reads the ref and the turn log in.
//!
//! `rechain`'s compare-and-swap is the answer for when the worktree lock is not
//! genuinely held — an older Sheep running alongside, or a lock broken as stale
//! while this process was frozen. It can only ever fire in that case, so it has
//! to be right in exactly that case, and whether it is depends on which of the
//! two reads happens first:
//!
//! * ref, then log — a turn appended between them is in the log the collection
//!   reads, and leaves the ref ahead of the value the swap will insist on. It
//!   is kept, or the collection refuses. Either way it survives.
//! * log, then ref — the same turn is missing from the log but already in the
//!   ref, so the swap *succeeds*, the rewrite drops the turn and the prune takes
//!   its objects. Silently, which is the exact failure the swap exists to stop.
//!
//! Two adjacent statements have no seam to test between, so this file makes
//! one: the `git` on `PATH` is a shim that stalls the first `rev-parse` of the
//! timeline's ref, which is the collection reading the ref. A writer that holds
//! no lock appends during that stall — the shape of an older Sheep beside this
//! one — and lands, deterministically, in the window whose ordering is in
//! question.
//!
//! It sets `PATH` for the whole process, so it lives in a test binary of its
//! own where there is nothing else running to disturb.

#![cfg(unix)]

use sheep::ops::{self, SnapMeta};
use sheep::repo::Worktree;
use sheep::shadow::Shadow;
use sheep::store::{Store, Turn, TurnKind};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BUDGET: usize = 60_000;
const LINE: &str = "default";
const TURNS: usize = 6;
const KEEP: usize = 3;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(args)
        .output()
        .expect("git should run");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A `git` that stalls the first read of the timeline's ref and passes
/// everything else straight through.
///
/// `mkdir` is the once-guard: it is one atomic syscall, so exactly one
/// invocation stalls however many run at the same time, and its success is also
/// the signal the test waits on.
fn install_shim(bin: &Path, marker: &Path, stall: Duration) {
    let real = String::from_utf8_lossy(
        &Command::new("/usr/bin/env").args(["which", "git"]).output().expect("which git").stdout,
    )
    .trim()
    .to_string();
    assert!(!real.is_empty(), "there must be a real git to fall through to");

    std::fs::create_dir_all(bin).unwrap();
    let script = format!(
        "#!/bin/sh\n\
         case \"$*\" in\n\
         *'rev-parse --verify --quiet refs/sheep/{LINE}'*)\n\
         \x20 if mkdir '{}' 2>/dev/null; then sleep {}; fi\n\
         \x20 ;;\n\
         esac\n\
         exec '{real}' \"$@\"\n",
        marker.display(),
        stall.as_secs_f64(),
    );
    let path = bin.join("git");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Record a turn the way a Sheep that does not know about the lock would:
/// mint a sequence number, commit the tree, append the row.
fn append_without_the_lock(wt: &Worktree, state: &Path, note: &str) -> Turn {
    let shadow = Shadow::ensure(wt.clone(), state).expect("shadow");
    let store = Store::open(state, &wt.id, LINE).expect("store");
    let tree = shadow.write_tree("snap").expect("write-tree");
    let mut turn = Turn {
        seq: store.next_seq().expect("next seq"),
        kind: TurnKind::Turn,
        commit: String::new(),
        tree: tree.clone(),
        parent: shadow.head(LINE).expect("head"),
        at: sheep::shadow::now(),
        files: 1,
        insertions: 0,
        deletions: 0,
        pane_id: None,
        agent: None,
        prompt: None,
        note: Some(note.to_string()),
    };
    let snapshot = shadow.commit(LINE, &tree, &turn.subject()).expect("commit");
    turn.commit = snapshot.commit;
    turn.at = snapshot.at;
    store.append(&turn).expect("append");
    turn
}

#[test]
fn a_turn_appended_while_a_collection_reads_is_never_silently_dropped() {
    let dir = TempDir::new().expect("tempdir");
    let base = std::fs::canonicalize(dir.path()).expect("canonicalize");
    let (repo, state) = (base.join("repo"), base.join("state"));
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    git(&repo, &["init", "--quiet", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "base\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "base"]);

    let wt = Worktree::discover(&repo).expect("worktree");
    for i in 0..TURNS {
        std::fs::write(repo.join("a.txt"), format!("revision {i}\n")).unwrap();
        ops::snap(
            &wt,
            &state,
            LINE,
            BUDGET,
            TurnKind::Turn,
            SnapMeta { note: Some(format!("turn {i}")), ..Default::default() },
            false,
        )
        .expect("snap")
        .expect("a turn");
    }

    // Everything past here goes through the shim.
    let marker = base.join("reading-the-ref");
    let bin = base.join("bin");
    install_shim(&bin, &marker, Duration::from_secs(2));
    let previous_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{previous_path}", bin.display()));

    let collecting = {
        let (wt, state) = (wt.clone(), state.clone());
        std::thread::spawn(move || {
            ops::collect(&wt, &state, LINE, ops::Retention { keep: KEEP, max_age_days: None }, true)
        })
    };

    // Wait for the collection to reach the ref, then append beside it.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(marker.exists(), "the collection never read the ref, so nothing was tested");

    std::fs::write(repo.join("a.txt"), "written beside the collection\n").unwrap();
    std::fs::write(repo.join("late.txt"), "and this too\n").unwrap();
    let late = append_without_the_lock(&wt, &state, "appended while the collection was reading");

    let collected = collecting.join().expect("the collecting thread should not panic");
    std::env::set_var("PATH", previous_path);

    // Two acceptable outcomes, and the third is the bug: the turn is kept, or
    // the collection refuses because the ref moved under it. What may never
    // happen is a collection that reports success having dropped it.
    let Ok(report) = collected else {
        return; // refused, loudly — the compare-and-swap did its job
    };

    let store = Store::open(&state, &wt.id, LINE).unwrap();
    let turns = store.all().unwrap();
    let kept = turns
        .iter()
        .find(|t| t.seq == late.seq)
        .unwrap_or_else(|| {
            panic!(
                "#{} was appended while the collection was reading and the collection reported success without it ({} kept, {} dropped): {:?}",
                late.seq, report.kept, report.dropped, turns
            )
        })
        .clone();

    assert_eq!(kept.tree, late.tree, "#{} must still hold the tree it recorded", late.seq);
    let shadow = Shadow::ensure(wt.clone(), &state).unwrap();
    let missing = shadow.verify(&kept.tree).expect("verify");
    assert!(missing.is_empty(), "#{} lost {} object(s): {missing:?}", late.seq, missing.len());
    assert_eq!(
        shadow.tree_of(&kept.commit).expect("its commit must still resolve"),
        kept.tree,
        "#{}'s commit must point at its tree",
        late.seq
    );
}
