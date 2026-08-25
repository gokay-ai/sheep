//! A thin, explicit wrapper over the `git` binary.
//!
//! Every git invocation Sheep makes goes through here, and every one of them is
//! scoped by an explicit `--git-dir` / `--work-tree` pair. We deliberately do
//! not inherit ambient git state from the environment, and we deliberately use
//! plumbing commands only: plumbing does not run hooks, so a repository's
//! `pre-commit` or `post-checkout` hook can never fire because Sheep took a
//! snapshot.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Environment variables that would silently redirect a git invocation
/// somewhere we did not intend. They are stripped from every child process.
const HOSTILE_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
];

#[derive(Clone, Debug)]
pub struct Git {
    git_dir: Option<PathBuf>,
    work_tree: Option<PathBuf>,
    index_file: Option<PathBuf>,
    cwd: PathBuf,
    envs: Vec<(String, String)>,
}

impl Git {
    /// Discovery mode: let git find the repository from `cwd`, the way the user
    /// would. Used to inspect the *user's* repository, never to write to it.
    pub fn discover(cwd: impl Into<PathBuf>) -> Self {
        Self { git_dir: None, work_tree: None, index_file: None, cwd: cwd.into(), envs: Vec::new() }
    }

    /// Explicit mode: operate on `git_dir` with `work_tree` checked out.
    pub fn scoped(git_dir: impl Into<PathBuf>, work_tree: impl Into<PathBuf>) -> Self {
        let work_tree = work_tree.into();
        Self {
            git_dir: Some(git_dir.into()),
            work_tree: Some(work_tree.clone()),
            index_file: None,
            cwd: work_tree,
            envs: Vec::new(),
        }
    }

    /// Bare mode: object-database operations with no working tree at all.
    pub fn bare(git_dir: impl Into<PathBuf>) -> Self {
        let git_dir = git_dir.into();
        Self {
            cwd: git_dir.clone(),
            git_dir: Some(git_dir),
            work_tree: None,
            index_file: None,
            envs: Vec::new(),
        }
    }

    /// Point this invocation at a scratch index file. Without it, `add` would
    /// write to the index inside `git_dir`; with it, the index is disposable.
    pub fn with_index(mut self, index: impl Into<PathBuf>) -> Self {
        self.index_file = Some(index.into());
        self
    }

    /// Set an environment variable for every invocation made through this
    /// handle. Used to pin the commit identity so Sheep works on a machine with
    /// no `user.email` configured.
    pub fn with_env(mut self, key: &str, value: impl Into<String>) -> Self {
        self.envs.push((key.to_string(), value.into()));
        self
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        for var in HOSTILE_ENV {
            cmd.env_remove(var);
        }
        if let Some(dir) = &self.git_dir {
            cmd.arg("--git-dir").arg(dir);
        }
        if let Some(tree) = &self.work_tree {
            cmd.arg("--work-tree").arg(tree);
        }
        if let Some(index) = &self.index_file {
            cmd.env("GIT_INDEX_FILE", index);
        }
        for (key, value) in &self.envs {
            cmd.env(key, value);
        }
        cmd.current_dir(&self.cwd);
        cmd.args(args);
        cmd.stdin(Stdio::null());
        cmd
    }

    /// Run and capture. Returns the raw `Output` without failing on a non-zero
    /// exit; callers that care about the exit code inspect it themselves.
    pub fn output(&self, args: &[&str]) -> Result<Output> {
        self.command(args)
            .output()
            .with_context(|| format!("failed to spawn `git {}`", args.join(" ")))
    }

    /// Run, require success, and return trimmed stdout.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        let out = self.output(args)?;
        if !out.status.success() {
            bail!(
                "git {} failed ({}): {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string())
    }

    /// Run and report only whether it succeeded.
    pub fn ok(&self, args: &[&str]) -> bool {
        self.output(args).map(|o| o.status.success()).unwrap_or(false)
    }

    /// Run, require success, and split stdout on NUL. Used with git's `-z`
    /// output so that filenames containing newlines cannot corrupt parsing.
    pub fn run_z(&self, args: &[&str]) -> Result<Vec<String>> {
        let out = self.output(args)?;
        if !out.status.success() {
            bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect())
    }
    /// Run with `input` on stdin, streaming both directions at once.
    ///
    /// The threading is not optional. Git's batch-oriented plumbing writes a
    /// line of output per line of input, so a single-threaded
    /// "write everything, then read everything" deadlocks the moment the
    /// child's stdout pipe buffer fills — around 64 KB, which a repository of
    /// a few thousand files reaches easily.
    pub fn run_stdin(&self, args: &[&str], input: &[u8]) -> Result<Output> {
        use std::io::Write;
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn `git {}`", args.join(" ")))?;

        let mut stdin = child.stdin.take().expect("stdin was piped");
        let payload = input.to_vec();
        let writer = std::thread::spawn(move || {
            let result = stdin.write_all(&payload).and_then(|()| stdin.flush());
            drop(stdin); // signal EOF so the child can finish
            result
        });

        let output = child
            .wait_with_output()
            .with_context(|| format!("failed waiting on `git {}`", args.join(" ")))?;

        match writer.join() {
            // A child that exits early leaves us writing into a closed pipe.
            // Its exit status is the real answer, so this is not an error.
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Ok(Err(e)) => {
                return Err(e)
                    .with_context(|| format!("failed writing stdin to `git {}`", args.join(" ")))
            }
            Ok(Ok(())) => {}
            Err(_) => bail!("the stdin writer thread for `git {}` panicked", args.join(" ")),
        }
        Ok(output)
    }
}

/// Absolute, symlink-resolved path. Two spellings of the same directory must
/// hash to the same repository id or we would keep two shadow repos for one
/// checkout.
pub fn canonical(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).with_context(|| format!("cannot resolve path {}", path.display()))
}
