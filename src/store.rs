//! The turn log.
//!
//! One append-only NDJSON file per timeline. NDJSON rather than SQLite on
//! purpose: no C dependency means the binary cross-compiles to every target
//! without a toolchain on the user's machine, and a corrupted tail costs one
//! line instead of a database.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnKind {
    /// An agent finished a turn.
    Turn,
    /// Taken automatically just before a restore, so undo is undoable.
    Checkpoint,
    /// The user asked for it.
    Manual,
}

impl TurnKind {
    pub fn label(&self) -> &'static str {
        match self {
            TurnKind::Turn => "turn",
            TurnKind::Checkpoint => "checkpoint",
            TurnKind::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    /// Monotonic within a timeline. This is the `#7` the user actually types.
    pub seq: u64,
    pub kind: TurnKind,
    pub commit: String,
    pub tree: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Unix seconds.
    pub at: u64,
    pub files: usize,
    pub insertions: u64,
    pub deletions: u64,
    /// Which herdr pane produced it, when Sheep is running under herdr.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// Which agent CLI produced it: claude, codex, opencode...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Best-effort, read off the pane. Always labelled as screen-scraped
    /// wherever it is shown, because it is not authoritative.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Turn {
    /// One-line summary, used as the shadow commit's subject so that
    /// `git log` on the shadow repo is readable on its own.
    pub fn subject(&self) -> String {
        let who = self.agent.as_deref().unwrap_or("-");
        format!(
            "#{} {} {} · {} files +{} -{}",
            self.seq,
            self.kind.label(),
            who,
            self.files,
            self.insertions,
            self.deletions
        )
    }
}

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn open(state_dir: &Path, worktree_id: &str, line: &str) -> Result<Self> {
        let dir = state_dir.join("turns").join(worktree_id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        Ok(Self { path: dir.join(format!("{}.ndjson", sanitize(line))) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every turn, oldest first. A malformed line is skipped rather than
    /// fatal — a truncated write must not make the whole history unreadable.
    pub fn all(&self) -> Result<Vec<Turn>> {
        let body = match std::fs::read_to_string(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", self.path.display())),
        };
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Turn>(l).ok())
            .collect())
    }

    pub fn next_seq(&self) -> Result<u64> {
        Ok(self.all()?.last().map(|t| t.seq + 1).unwrap_or(1))
    }

    pub fn append(&self, turn: &Turn) -> Result<()> {
        let mut line = serde_json::to_string(turn)?;
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("cannot open {}", self.path.display()))?;
        file.write_all(line.as_bytes())?;
        file.flush()?;
        Ok(())
    }

    pub fn find(&self, seq: u64) -> Result<Option<Turn>> {
        Ok(self.all()?.into_iter().find(|t| t.seq == seq))
    }
}

fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    if cleaned.is_empty() { "default".into() } else { cleaned }
}
