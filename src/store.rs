//! The turn log.
//!
//! One append-only NDJSON file per timeline. NDJSON rather than SQLite on
//! purpose: no C dependency means the binary cross-compiles to every target
//! without a toolchain on the user's machine, and a corrupted tail costs one
//! line instead of a database.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
        Ok(Self { path: dir.join(format!("{}.ndjson", slug(line))) })
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
            Err(e) => {
                return Err(e).with_context(|| format!("cannot read {}", self.path.display()))
            }
        };
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Turn>(l).ok())
            .collect())
    }

    /// The most recently appended turn, without reading the whole log.
    ///
    /// The recorder calls this on every snapshot and runs for days, so parsing
    /// the entire timeline each time is quadratic over the life of the daemon.
    /// The last record is almost always within the final few kilobytes, so read
    /// backwards in blocks and only fall back to a full parse if the tail turns
    /// out to hold nothing readable.
    pub fn last(&self) -> Result<Option<Turn>> {
        use std::io::{Read, Seek, SeekFrom};

        let mut file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| format!("cannot read {}", self.path.display()))
            }
        };
        let len = file.metadata()?.len();
        if len == 0 {
            return Ok(None);
        }

        const BLOCK: u64 = 8 * 1024;
        let mut span = BLOCK.min(len);
        loop {
            file.seek(SeekFrom::Start(len - span))?;
            let mut buf = vec![0u8; span as usize];
            file.read_exact(&mut buf)?;
            let text = String::from_utf8_lossy(&buf);

            // Only trust a line we know is whole: unless we are at the start of
            // the file, the first line in the block may be a fragment.
            let complete = if span == len {
                text.as_ref()
            } else {
                text.split_once('\n').map_or("", |(_, rest)| rest)
            };
            if let Some(turn) =
                complete.lines().rev().filter_map(|l| serde_json::from_str::<Turn>(l).ok()).next()
            {
                return Ok(Some(turn));
            }
            if span == len {
                return Ok(self.all()?.pop());
            }
            span = (span * 4).min(len);
        }
    }

    pub fn next_seq(&self) -> Result<u64> {
        Ok(self.last()?.map(|t| t.seq + 1).unwrap_or(1))
    }

    /// Replace the whole log. Written to a sibling and renamed, so an
    /// interrupted prune leaves the previous timeline intact rather than half
    /// a file.
    pub fn rewrite(&self, turns: &[Turn]) -> Result<()> {
        let tmp = self.path.with_extension("ndjson.tmp");
        let mut body = String::new();
        for turn in turns {
            body.push_str(&serde_json::to_string(turn)?);
            body.push('\n');
        }
        std::fs::write(&tmp, body).with_context(|| format!("cannot write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("cannot replace {}", self.path.display()))?;
        Ok(())
    }

    /// Every timeline recorded for one worktree.
    pub fn lines_for(state_dir: &Path, worktree_id: &str) -> Result<Vec<String>> {
        let dir = state_dir.join("turns").join(worktree_id);
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return Ok(Vec::new()),
        };
        let mut lines: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                (path.extension().and_then(|x| x.to_str()) == Some("ndjson"))
                    .then(|| path.file_stem()?.to_str().map(str::to_string))
                    .flatten()
            })
            .collect();
        lines.sort();
        Ok(lines)
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

/// A timeline name reduced to something both a filesystem and a git ref accept.
///
/// A timeline has to name two things: a file in the turn log and a ref in the
/// shadow repository. Herdr pane ids — the natural timeline name when an agent
/// is being recorded — contain a colon, which git flatly refuses in a ref name:
///
/// ```text
/// fatal: update_ref failed for ref 'refs/sheep/w31:pW': refusing to update ref with bad name
/// ```
///
/// So everything outside `[A-Za-z0-9_-]` becomes `-`. That mapping is lossy, so
/// a name that had to be changed carries a short digest of the original: two
/// different timelines can then never collapse onto the same ref, which would
/// silently interleave two agents' histories.
///
/// Both [`Store`] and the shadow repository call this, so they cannot disagree
/// about which timeline a pane owns.
pub fn slug(line: &str) -> String {
    let cleaned: String = line
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    if cleaned.is_empty() {
        return "default".into();
    }
    // Only names that are already legal pass through unchanged. A leading dash
    // is excluded even when nothing else had to be rewritten, because a bare
    // `-name` is read as a flag by half the commands that will ever see it.
    if cleaned == line && !cleaned.starts_with('-') {
        return cleaned;
    }
    let mut hasher = Sha256::new();
    hasher.update(line.as_bytes());
    let digest = hasher.finalize();
    let suffix: String = digest.iter().take(3).map(|b| format!("{b:02x}")).collect();
    // A name made entirely of separators trims to nothing, and a ref or a
    // filename beginning with `-` is a trap for every command that takes flags.
    let base = match cleaned.trim_matches('-') {
        "" => "line",
        base => base,
    };
    format!("{base}-{suffix}")
}

#[cfg(test)]
mod tests {
    use super::slug;

    #[test]
    fn a_plain_name_is_left_alone() {
        assert_eq!(slug("default"), "default");
        assert_eq!(slug("my_line-2"), "my_line-2");
    }

    #[test]
    fn a_herdr_pane_id_becomes_a_legal_git_ref() {
        // The colon is what git rejects; this is the case that matters.
        let s = slug("w31:pW");
        assert!(s.starts_with("w31-pW-"), "expected a sanitised pane id, got {s}");
        assert!(!s.contains(':'));
    }

    #[test]
    fn two_names_that_clean_to_the_same_thing_stay_apart() {
        // Without the digest these would both be `w3-p1` and two agents would
        // write into one history.
        assert_ne!(slug("w3:p1"), slug("w3/p1"));
        assert_ne!(slug("w3:p1"), "w3-p1");
    }

    #[test]
    fn a_name_made_of_separators_still_produces_a_usable_ref() {
        assert_eq!(slug(""), "default");
        for awkward in ["///", "-", "...", "@{"] {
            let s = slug(awkward);
            assert!(!s.is_empty(), "{awkward:?} produced an empty slug");
            assert!(
                !s.starts_with('-'),
                "{awkward:?} produced {s:?}, and a leading dash is a trap for every command that takes flags"
            );
            assert!(
                s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "{awkward:?} produced {s:?}, which git will not accept as a ref"
            );
        }
    }
}
