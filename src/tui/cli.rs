//! `sheep ui` — the terminal interface's command-line surface and runtime.
//!
//! The runtime is deliberately thin: it owns the terminal, translates crossterm
//! key events into [`Key`], pumps [`Job`]s to the worker and [`Reply`]s back
//! into the [`App`], and draws. Everything that decides anything lives in
//! `app.rs`; everything slow lives in `engine.rs`.

use crate::herdr::wire;
use crate::repo::{self, Worktree, DEFAULT_MAX_FILES};
use crate::shadow;
use crate::tui::app::{App, Fatal, Key};
use crate::tui::engine::{self, Ctx, Worker};
use crate::tui::render;
use crate::tui::runtime;
use crate::tui::text;
use anyhow::{Context, Result};
use clap::Args;
use crossterm::event::{self, Event, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::Terminal;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

/// How often the worker re-reads the turn log on its own, so a dock left open
/// beside a working agent grows new turns without anyone pressing a key.
const POLL: Duration = Duration::from_millis(900);

#[derive(Args, Debug, Clone)]
pub struct UiArgs {
    /// Open straight into the rewind picker rather than the timeline dock.
    #[arg(long)]
    pub rewind: bool,
    /// Do not tell the agent what a restore took back.
    #[arg(long)]
    pub no_notify: bool,
    /// Start with this turn selected, e.g. `--select 7`.
    #[arg(long, value_name = "TURN")]
    pub select: Option<String>,
    /// Feed these keys to the interface before drawing, e.g. `--keys jjd`.
    ///
    /// A scripted driver for `--snapshot` and for tests. `R` is ignored: a
    /// restore is only ever reachable from a plan somebody looked at, and a
    /// string on a command line is not that.
    #[arg(long, value_name = "KEYS")]
    pub keys: Option<String>,
    /// Render one frame as plain text and exit: `--snapshot 100x34`.
    ///
    /// The interface is the product's front page, so it has to be reviewable in
    /// a pull request and assertable in CI without a pseudo-terminal.
    #[arg(long, value_name = "COLSxROWS")]
    pub snapshot: Option<String>,
}

pub fn run(args: &UiArgs, repo_arg: Option<&Path>, line: &str) -> Result<()> {
    let cwd = match repo_arg {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("cannot read the current directory")?,
    };

    let mut app = App::new(text::basename(&cwd), cwd.display().to_string(), line);
    app.inside_herdr = wire::inside_herdr();
    app.notify = !args.no_notify;

    let ctx = match open(&cwd, line) {
        Ok(ctx) => ctx,
        Err(fatal) => {
            let dead = app.dead(fatal);
            return match &args.snapshot {
                Some(size) => print_snapshot(&dead, size),
                None => interact(dead, None),
            };
        }
    };
    app.repo = text::basename(&ctx.wt.root);
    app.root = ctx.wt.root.display().to_string();

    match &args.snapshot {
        Some(size) => {
            settle(&ctx, &mut app, args);
            print_snapshot(&app, size)
        }
        None => {
            app.reload();
            if args.select.is_some() || args.rewind || args.keys.is_some() {
                // Selecting a turn or opening a plan needs the timeline in hand
                // first; do that one job synchronously so the first frame is
                // already the view the user asked for.
                settle(&ctx, &mut app, args);
            }
            interact(app, Some(ctx))
        }
    }
}

/// Discover the worktree and the state directory, or explain why not.
fn open(cwd: &Path, line: &str) -> std::result::Result<Ctx, Fatal> {
    let wt = Worktree::discover(cwd).map_err(|e| Fatal {
        headline: "not a git worktree".into(),
        detail: format!("{e:#}"),
        remedy: vec![
            "cd into a checkout, or run `git init` here.".into(),
            "`sheep doctor` reports whether a worktree is safe to record.".into(),
        ],
    })?;
    let state = repo::state_dir().map_err(|e| Fatal {
        headline: "no state directory".into(),
        detail: format!("{e:#}"),
        remedy: vec!["set SHEEP_STATE_DIR to a directory Sheep may write to.".into()],
    })?;
    Ok(Ctx { wt, state, line: line.to_string(), max_files: DEFAULT_MAX_FILES })
}

/// Bring the app to the state the flags describe, running every job on this
/// thread. Used for `--snapshot` and for the first frame of `--rewind`, where
/// there is no point drawing a loading state nobody will see.
///
/// Public so a test can drive the whole `UiArgs` surface — including the one
/// thing standing between `--keys` and a write nobody authorised.
pub fn settle(ctx: &Ctx, app: &mut App, args: &UiArgs) {
    app.reload();
    pump(ctx, app);
    if let Some(target) = &args.select {
        select_turn(app, target);
    }
    if args.rewind {
        app.open_rewind();
        pump(ctx, app);
    }
    if let Some(keys) = &args.keys {
        for ch in keys.chars() {
            let Some(key) = scripted_key(ch) else { continue };
            app.on_key(key);
            pump(ctx, app);
        }
    }
}

/// Translate one character of `--keys`, or refuse it.
///
/// `R` is the only key that writes, and it is refused here: a restore is
/// reachable only from a plan a human has looked at, and a string on a command
/// line is not that. `--keys` runs before the first frame is drawn, so letting
/// `R` through would rewrite a worktree with nothing ever shown to anyone.
pub fn scripted_key(ch: char) -> Option<Key> {
    match ch {
        'R' => None,
        '\n' | '\r' => Some(Key::Enter),
        '\x1b' => Some(Key::Esc),
        other => Some(Key::Char(other)),
    }
}

/// Run queued jobs until the app stops asking for more. The bound is a guard
/// against a job that keeps queuing another, not an expected limit.
fn pump(ctx: &Ctx, app: &mut App) {
    for _ in 0..8 {
        let jobs = app.take_jobs();
        if jobs.is_empty() {
            break;
        }
        for job in jobs {
            app.apply(engine::execute(ctx, job));
        }
    }
}

fn select_turn(app: &mut App, target: &str) {
    if let Ok(seq) = target.trim_start_matches('#').parse::<u64>() {
        if let Some(index) = app.turns.iter().position(|t| t.seq == seq) {
            for _ in 0..index {
                app.on_key(Key::Down);
            }
        }
    }
}

// ------------------------------------------------------------------ snapshot

fn parse_size(spec: &str) -> (u16, u16) {
    let mut parts = spec.split(['x', 'X']);
    let w = parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(100);
    let h = parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(34);
    (w.clamp(20, 400), h.clamp(8, 200))
}

/// Draw one frame into an off-screen buffer and write it to stdout as text.
fn print_snapshot(app: &App, spec: &str) -> Result<()> {
    let (w, h) = parse_size(spec);
    let mut terminal = Terminal::new(TestBackend::new(w, h))?;
    terminal.draw(|frame| render::draw(frame, app))?;

    let buffer = terminal.backend().buffer();
    let mut out = std::io::stdout().lock();
    for y in 0..buffer.area.height {
        let mut row = String::new();
        for x in 0..buffer.area.width {
            row.push_str(buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        writeln!(out, "{}", row.trim_end())?;
    }
    Ok(())
}

// --------------------------------------------------------------- interactive

fn interact(mut app: App, ctx: Option<Ctx>) -> Result<()> {
    let mut screen = Frames(enter()?);
    let worker = ctx.map(|ctx| Worker::spawn(ctx, POLL));
    let result = runtime::run(&mut app, &mut screen, &mut Keyboard, &worker, &shadow::now);
    // Put the terminal back whatever happened, but never let a failure to do so
    // hide the reason the loop stopped.
    let restored = leave(&mut screen.0);
    result.and(restored)
}

/// The terminal, as the loop sees it.
struct Frames(Term);

impl runtime::Screen for Frames {
    fn render(&mut self, app: &App) -> Result<()> {
        self.0.draw(|frame| render::draw(frame, app))?;
        Ok(())
    }
}

/// The keyboard, as the loop sees it.
struct Keyboard;

impl runtime::Input for Keyboard {
    fn wait(&mut self, timeout: Duration) -> Result<Option<Key>> {
        if !event::poll(timeout)? {
            return Ok(None);
        }
        Ok(match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => translate(key),
            _ => None,
        })
    }

    fn drain(&mut self) -> Result<usize> {
        let mut dropped = 0;
        while event::poll(Duration::ZERO)? {
            let _ = event::read()?;
            dropped += 1;
        }
        Ok(dropped)
    }

    fn pause(&mut self, timeout: Duration) {
        std::thread::sleep(timeout);
    }
}

fn translate(ev: KeyEvent) -> Option<Key> {
    use crossterm::event::KeyCode as C;
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        return match ev.code {
            C::Char('c') | C::Char('d') => Some(Key::Char('q')),
            _ => None,
        };
    }
    Some(match ev.code {
        C::Up => Key::Up,
        C::Down => Key::Down,
        C::Left => Key::Left,
        C::Right => Key::Right,
        C::PageUp => Key::PageUp,
        C::PageDown => Key::PageDown,
        C::Home => Key::Home,
        C::End => Key::End,
        C::Enter => Key::Enter,
        C::Esc => Key::Esc,
        C::Char(c) => Key::Char(c),
        _ => return None,
    })
}

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

fn enter() -> Result<Term> {
    enable_raw_mode().context("cannot put the terminal into raw mode")?;
    std::io::stdout().execute(EnterAlternateScreen)?;
    // A panic in raw mode leaves a terminal nobody can type into. Put it back
    // before the message is printed, whatever happens.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = std::io::stdout().execute(LeaveAlternateScreen);
        previous(info);
    }));
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    terminal.hide_cursor()?;
    terminal.clear()?;
    Ok(terminal)
}

fn leave(terminal: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    std::io::stdout().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
