//! Drawing. Pure: a [`Frame`] and an [`App`] in, pixels out, no I/O.
//!
//! Two rules shape everything here.
//!
//! * **Forty columns is a real width.** A dock lives beside an agent pane, so
//!   every row is composed against the width it was given and degrades by
//!   dropping detail rather than by overflowing. Nothing is laid out with a
//!   hardcoded column.
//! * **The plan is the product.** The rewind overlay spends its space on the
//!   file list and on a footer that says, in words, what the next keystroke
//!   will do to the disk.

use crate::store::{Turn, TurnKind};
use crate::tui::app::{App, Fatal, Level, Mode, PatchState, PlanState, Status};
use crate::tui::engine::{Action, PlanView};
use crate::tui::text;
use crate::tui::theme;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

const SPINNER: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if let Some(fatal) = &app.fatal {
        draw_fatal(frame, area, fatal);
        return;
    }
    draw_dock(frame, area, app);
    if app.mode == Mode::Rewind {
        draw_rewind(frame, area, app);
    }
    if app.mode == Mode::Help {
        draw_help(frame, area);
    }
}

// ------------------------------------------------------------------ helpers

/// One row with content pinned to both edges. Padding is computed from the
/// widths actually used, which is the only way to right-align inside a
/// `ListItem` — the list widget has no notion of alignment.
fn edges(width: usize, left: Vec<Span<'static>>, right: Vec<Span<'static>>) -> Line<'static> {
    let used: usize = left.iter().chain(right.iter()).map(|s| text::width(&s.content)).sum();
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    spans.extend(right);
    Line::from(spans)
}

fn pad(s: &str, to: usize) -> String {
    let mut out = text::truncate(s, to);
    while text::width(&out) < to {
        out.push(' ');
    }
    out
}

/// Keep the tail of a path: `render.rs` identifies a file, `src/tui/` does not.
fn truncate_left(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let n = text::width(s);
    if n <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - (max - 1)).collect();
    format!("…{tail}")
}

/// As many `key label` pairs as fit, dropped from the right when they do not.
fn hints(width: usize, pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for (key, label) in pairs {
        let sep = if spans.is_empty() { 0 } else { 3 };
        let cost = sep + text::width(key) + 1 + text::width(label);
        if used + cost > width {
            break;
        }
        if sep > 0 {
            spans.push(Span::styled(" · ", theme::dim()));
        }
        spans.push(Span::styled((*key).to_string(), theme::strong()));
        spans.push(Span::styled(format!(" {label}"), theme::dim()));
        used += cost;
    }
    Line::from(spans)
}

fn spinner(app: &App) -> &'static str {
    SPINNER[app.spinner % SPINNER.len()]
}

fn rule(width: usize) -> Line<'static> {
    Line::from(Span::styled("─".repeat(width), theme::dim()))
}

fn kind_label(kind: TurnKind, compact: bool) -> &'static str {
    match (kind, compact) {
        (TurnKind::Turn, _) => "turn",
        (TurnKind::Checkpoint, false) => "checkpoint",
        (TurnKind::Checkpoint, true) => "ckpt",
        (TurnKind::Manual, false) => "manual",
        (TurnKind::Manual, true) => "man",
    }
}

// --------------------------------------------------------------------- dock

fn draw_dock(frame: &mut Frame, area: Rect, app: &App) {
    let status_lines = status_block(app, area.width.saturating_sub(2) as usize);
    let banner_lines = banner(app, area.width.saturating_sub(2) as usize);
    let [head, warn, body, note, keys] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(banner_lines.len() as u16),
        Constraint::Min(3),
        Constraint::Length(status_lines.len() as u16),
        Constraint::Length(1),
    ])
    .areas(area);

    frame.render_widget(Paragraph::new(header(app, head.width as usize)), head);
    if !banner_lines.is_empty() {
        frame.render_widget(Paragraph::new(banner_lines), warn);
    }
    draw_timeline(frame, body, app);
    if !status_lines.is_empty() {
        frame.render_widget(Paragraph::new(status_lines), note);
    }
    frame.render_widget(
        Paragraph::new(hints(
            keys.width as usize,
            &[
                ("j/k", "move"),
                ("enter", "rewind"),
                ("?", "keys"),
                ("q", "quit"),
                ("n", "notify"),
                ("r", "refresh"),
            ],
        )),
        keys,
    );
}

fn header(app: &App, width: usize) -> Vec<Line<'static>> {
    // A tree nobody can vouch for outranks "busy": the reload that follows a
    // failed restore must not replace the warning with a spinner.
    let health: Vec<Span<'static>> = if app.uncertain.is_some() {
        vec![Span::styled("unsafe", theme::danger())]
    } else if app.loading {
        vec![Span::styled(format!("{} reading", spinner(app)), theme::dim())]
    } else if !app.blockers.is_empty() {
        vec![Span::styled("blocked", theme::danger())]
    } else if !app.warnings.is_empty() {
        vec![Span::styled("ready", theme::ok()), Span::styled(" · notes", theme::warn())]
    } else {
        vec![Span::styled("ready", theme::ok())]
    };
    let health_width: usize = health.iter().map(|s| text::width(&s.content)).sum();

    let name_budget = width.saturating_sub(9 + health_width + 2);
    let title = vec![
        Span::styled(" sheep ", theme::badge()),
        Span::raw(" "),
        Span::styled(text::truncate(&app.repo, name_budget), theme::strong()),
    ];

    let newest = app.turns.first().map(|t| {
        if width < 56 {
            text::age_short(app.now, t.at)
        } else {
            text::age(app.now, t.at)
        }
    });
    let mut facts = vec![format!("timeline {}", app.line), plural(app.turns.len(), "turn")];
    if let Some(age) = newest {
        facts.push(format!("newest {age}"));
    }
    facts.push(match (app.inside_herdr, app.notify) {
        (true, true) => "notify on".into(),
        (true, false) => "notify off".into(),
        (false, _) => "standalone".into(),
    });

    // Drop whole facts rather than truncating one mid-word: half a word is
    // noise, and the facts are already ordered by how much they matter.
    while facts.len() > 1 && text::width(&facts.join(" · ")) > width {
        facts.pop();
    }
    vec![
        edges(width, title, health),
        Line::from(Span::styled(text::truncate(&facts.join(" · "), width), theme::dim())),
    ]
}

/// A blocker is the difference between "Sheep is idle" and "Sheep cannot run".
/// It gets its own band above the timeline rather than a line in a footer.
fn banner(app: &App, width: usize) -> Vec<Line<'static>> {
    // A tree that is between two states outranks anything else the dock could
    // say about itself, and it stays up until a restore puts it right.
    let (heading, detail) = match (&app.uncertain, app.blockers.first()) {
        (Some(why), _) => ("this worktree is between two states", why),
        (None, Some(blocker)) => ("cannot record or restore", blocker),
        (None, None) => return Vec::new(),
    };
    let mut lines = vec![Line::from(vec![
        Span::styled("▌", theme::danger()),
        Span::styled(format!(" {heading} "), theme::danger()),
    ])];
    for chunk in text::wrap(detail, width.saturating_sub(2)).into_iter().take(4) {
        lines.push(Line::from(vec![
            Span::styled("▌", theme::danger()),
            Span::styled(format!(" {chunk}"), theme::plain()),
        ]));
    }
    lines
}

fn status_block(app: &App, width: usize) -> Vec<Line<'static>> {
    let Some(Status { level, lines }) = &app.status else { return Vec::new() };
    let (bar, first) = match level {
        Level::Good => (theme::ok(), theme::ok()),
        Level::Bad => (theme::danger(), theme::danger()),
        Level::Info => (theme::accent(), theme::plain()),
    };
    // A failure gets the room to finish its sentence. Cutting one off loses the
    // way back — `sheep restore #N --yes` is the last clause of the message
    // that matters most, and a five-line cap ate it.
    let cap = if *level == Level::Bad { 9 } else { 3 };
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let style = if i == 0 { first } else { theme::dim() };
        for chunk in text::wrap(line, width.saturating_sub(2)) {
            out.push(Line::from(vec![
                Span::styled("▌", bar),
                Span::styled(format!(" {chunk}"), style),
            ]));
            if out.len() >= cap {
                return out;
            }
        }
    }
    out
}

fn draw_timeline(frame: &mut Frame, area: Rect, app: &App) {
    let position = if app.turns.is_empty() {
        String::new()
    } else {
        format!(" {}/{} ", app.sel + 1, app.turns.len())
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::dim())
        .title(Span::styled(" timeline ", theme::accent()))
        .title_bottom(Line::from(Span::styled(position, theme::dim())).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.turns.is_empty() {
        frame.render_widget(
            Paragraph::new(empty_state(app, inner.width as usize)).wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }

    let width = inner.width as usize;
    let peak = app.turns.iter().map(|t| t.insertions + t.deletions).max().unwrap_or(0);
    let items: Vec<ListItem> = app
        .turns
        .iter()
        .enumerate()
        .map(|(i, turn)| ListItem::new(turn_rows(turn, i == app.sel, app, width, peak)))
        .collect();

    let mut state = ListState::default().with_selected(Some(app.sel));
    frame.render_stateful_widget(List::new(items), inner, &mut state);
}

/// One turn, as two to five lines depending on what it knows and whether it is
/// the row the user is on.
fn turn_rows(
    turn: &Turn,
    selected: bool,
    app: &App,
    width: usize,
    peak: u64,
) -> Vec<Line<'static>> {
    let avail = width.saturating_sub(1);
    let compact = avail < 46;
    let gutter = if selected {
        Span::styled(theme::CURSOR, theme::cursor_style())
    } else {
        Span::raw(theme::GUTTER)
    };
    let body = if selected { theme::strong() } else { theme::plain() };

    // --- identity. On a wide dock the snapshot id fills the gap the age
    // leaves; on a narrow one it is the first thing dropped.
    let age = if compact { text::age_short(app.now, turn.at) } else { text::age(app.now, turn.at) };
    let tail =
        if avail >= 62 { format!("{} · {age}", text::abbrev(&turn.commit, 8)) } else { age };
    let kind_width = if compact { 5 } else { 11 };
    let agent_budget =
        avail.saturating_sub((if compact { 4 } else { 5 }) + kind_width + text::width(&tail) + 1);
    let agent = turn.agent.clone().unwrap_or_default();
    let first = edges(
        avail,
        vec![
            Span::styled(
                pad(&format!("#{}", turn.seq), if compact { 4 } else { 5 }),
                theme::accent_strong().patch(body),
            ),
            Span::styled(pad(kind_label(turn.kind, compact), kind_width), theme::kind(turn.kind)),
            Span::styled(text::truncate(&agent, agent_budget), body),
        ],
        vec![Span::styled(tail, theme::dim())],
    );

    // --- shape of the change, as a number and as a picture. The bar sits with
    // the counts rather than at the far edge: it is the same fact twice, and
    // splitting them across the row reads as two unrelated columns.
    let slots = match avail {
        0..=29 => 0,
        30..=39 => 5,
        40..=51 => 8,
        _ => 12,
    };
    let (adds, dels) = text::magnitude(turn.insertions, turn.deletions, peak, slots);
    // The first turn on a timeline has nothing to diff against, so a `+0 −0`
    // would be a lie about a snapshot that captured the whole tree.
    let stats = if turn.parent.is_some() && turn.files == 0 {
        // A checkpoint taken when the tree had not moved since the last turn.
        // `0 files +0 −0` is three ways of saying nothing at all.
        vec![Span::styled(
            text::truncate("no change since the previous turn", avail.saturating_sub(2)),
            theme::dim(),
        )]
    } else if turn.parent.is_none() {
        vec![Span::styled(
            text::truncate(
                &format!("{} captured — the starting point", files(turn.files)),
                avail.saturating_sub(2),
            ),
            theme::dim(),
        )]
    } else {
        vec![
            Span::styled(files(turn.files), theme::dim()),
            Span::raw("   "),
            Span::styled(format!("+{}", turn.insertions), theme::added()),
            Span::raw(" "),
            Span::styled(format!("−{}", turn.deletions), theme::removed()),
            Span::raw("   "),
            Span::styled("█".repeat(adds), theme::added()),
            Span::styled("█".repeat(dels), theme::removed()),
        ]
    };
    let mut second = vec![Span::raw("  ")];
    second.extend(stats);
    let second = Line::from(second);

    let mut rows = vec![prefix(gutter.clone(), first), prefix(gutter.clone(), second)];

    // --- what the human asked for, when we managed to read it
    let prompt = turn.prompt.as_deref().map(text::clean).filter(|p| !p.is_empty());
    if let Some(prompt) = &prompt {
        rows.push(prefix(
            gutter.clone(),
            Line::from(Span::styled(
                text::truncate(&format!("  “{prompt}”"), avail),
                theme::quiet_italic(),
            )),
        ));
        if selected {
            rows.push(prefix(
                gutter.clone(),
                Line::from(Span::styled(
                    text::truncate("    read off the pane — not authoritative", avail),
                    theme::dim(),
                )),
            ));
        }
    } else if let Some(note) = &turn.note {
        rows.push(prefix(
            gutter.clone(),
            Line::from(Span::styled(text::truncate(&format!("  {note}"), avail), theme::dim())),
        ));
    }

    if selected {
        let stamp = format!("  {} · {}", crate::ops::short(&turn.commit), text::stamp(turn.at));
        rows.push(prefix(
            gutter,
            Line::from(Span::styled(text::truncate(&stamp, avail), theme::dim())),
        ));
    }
    rows
}

fn files(n: usize) -> String {
    plural(n, "file")
}

/// `1 turn`, `4 turns`. A dock that says "1 turns" reads as unfinished.
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

fn prefix(gutter: Span<'static>, line: Line<'static>) -> Line<'static> {
    let mut spans = vec![gutter];
    spans.extend(line.spans);
    Line::from(spans)
}

fn empty_state(app: &App, width: usize) -> Vec<Line<'static>> {
    if app.loading {
        return vec![
            Line::raw(""),
            Line::from(Span::styled(
                format!(" {} reading the timeline…", spinner(app)),
                theme::dim(),
            )),
        ];
    }
    let mut lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            text::truncate(&format!(" nothing recorded on `{}` yet", app.line), width),
            theme::strong(),
        )),
        Line::raw(""),
    ];
    for (cmd, what) in
        [("sheep snap", "record the tree as it is now"), ("sheep watch", "record every agent turn")]
    {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(pad(cmd, 13), theme::accent()),
            Span::styled(text::truncate(what, width.saturating_sub(15)), theme::dim()),
        ]));
    }
    lines.push(Line::raw(""));
    for chunk in text::wrap("Sheep records nothing until one of those runs — it never watches your files behind your back.", width.saturating_sub(2)) {
        lines.push(Line::from(Span::styled(format!(" {chunk}"), theme::dim())));
    }
    lines
}

// ------------------------------------------------------------------- rewind

/// Full width, starting under the dock's header.
///
/// An overlay inset by a column or two leaves single-column slivers of the
/// timeline down each side, which reads as a rendering bug rather than as
/// depth. Keeping the header visible is enough to say "still inside sheep".
fn overlay_rect(area: Rect) -> Rect {
    let top = if area.height >= 14 { 2 } else { 0 };
    Rect { x: area.x, y: area.y + top, width: area.width, height: area.height - top }
}

fn draw_rewind(frame: &mut Frame, area: Rect, app: &App) {
    let rect = overlay_rect(area);
    frame.render_widget(Clear, rect);

    let title = match &app.plan {
        PlanState::Ready(plan) => format!(" rewind to #{} ", plan.seq),
        PlanState::Loading(seq) | PlanState::Failed { seq, .. } => format!(" rewind to #{seq} "),
        PlanState::Idle => " rewind ".to_string(),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::accent())
        .title(Span::styled(title, theme::accent_strong()))
        .title_bottom(
            Line::from(if app.restoring {
                // The frame that says "restoring…" in its body must not say
                // "nothing is written yet" on its border.
                Span::styled(" writing — do not interrupt ", theme::danger())
            } else if app.uncertain.is_some() {
                // Nor may it go back to promising a dry run once a write has
                // already half-happened.
                Span::styled(" this worktree is between two states ", theme::danger())
            } else {
                Span::styled(" dry run — nothing is written yet ", theme::dim())
            })
            .right_aligned(),
        );
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let width = inner.width as usize;
    let head = headline(app, width);
    let foot = footer(app, width);
    let [head_area, body_area, foot_area] = Layout::vertical([
        Constraint::Length(head.len() as u16),
        Constraint::Min(2),
        Constraint::Length(foot.len() as u16),
    ])
    .areas(inner);

    frame.render_widget(Paragraph::new(head), head_area);
    match &app.plan {
        PlanState::Ready(plan) => draw_plan_body(frame, body_area, app, plan),
        PlanState::Loading(_) => frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {} working out what would change…", spinner(app)),
                theme::dim(),
            ))),
            body_area,
        ),
        PlanState::Failed { message, .. } => {
            let mut lines = vec![Line::raw("")];
            for chunk in text::wrap(message, width.saturating_sub(2)) {
                lines.push(Line::from(vec![
                    Span::styled("▌", theme::danger()),
                    Span::styled(format!(" {chunk}"), theme::plain()),
                ]));
            }
            frame.render_widget(Paragraph::new(lines), body_area);
        }
        PlanState::Idle => {}
    }
    frame.render_widget(Paragraph::new(foot), foot_area);
}

fn headline(app: &App, width: usize) -> Vec<Line<'static>> {
    let turn = app.selected();
    // Ordered by what survives a truncation: when this turn happened matters
    // more than which agent produced it, which matters more than its kind.
    let subtitle = turn
        .map(|t| {
            let mut parts = vec![text::age(app.now, t.at)];
            if let Some(agent) = &t.agent {
                parts.push(agent.clone());
            }
            parts.push(kind_label(t.kind, false).to_string());
            parts.join(" · ")
        })
        .unwrap_or_default();

    // The snapshot id is the least useful thing on this line, so it is the
    // first to go when the panel is narrow.
    let commit = match (width >= 58, &app.plan) {
        (false, _) => String::new(),
        (true, PlanState::Ready(plan)) => crate::ops::short(&plan.commit).to_string(),
        (true, _) => turn.map(|t| crate::ops::short(&t.commit).to_string()).unwrap_or_default(),
    };

    let anchor = format!("back to #{}  ", turn.map(|t| t.seq).unwrap_or(0));
    let budget = width.saturating_sub(text::width(&anchor) + text::width(&commit) + 2);
    let first = edges(
        width,
        vec![
            Span::styled(anchor, theme::accent_strong()),
            Span::styled(text::truncate(&subtitle, budget), theme::dim()),
        ],
        vec![Span::styled(commit, theme::dim())],
    );

    // What the agent was asked, on the screen where you decide to take it back.
    let asked = turn.and_then(|t| {
        t.prompt
            .as_deref()
            .map(text::clean)
            .filter(|p| !p.is_empty())
            .map(|p| (format!("“{p}”"), theme::quiet_italic()))
            .or_else(|| t.note.clone().map(|n| (n, theme::dim())))
    });

    let counts = match &app.plan {
        PlanState::Ready(plan) if plan.is_noop() => {
            Line::from(Span::styled("the working tree already matches this turn", theme::warn()))
        }
        PlanState::Ready(plan) if width >= 46 => Line::from(vec![
            Span::styled(format!("{} paths change", plan.touched()), theme::strong()),
            Span::styled("  —  ", theme::dim()),
            Span::styled(format!("{} written", plan.written), theme::added()),
            Span::styled(" · ", theme::dim()),
            Span::styled(format!("{} removed", plan.removed), theme::removed()),
        ]),
        PlanState::Ready(plan) => Line::from(vec![
            Span::styled(format!("{} paths  ", plan.touched()), theme::strong()),
            Span::styled(format!("{} write", plan.written), theme::added()),
            Span::styled(" · ", theme::dim()),
            Span::styled(format!("{} remove", plan.removed), theme::removed()),
        ]),
        PlanState::Loading(_) => Line::from(Span::styled("planning…", theme::dim())),
        PlanState::Failed { .. } => {
            Line::from(Span::styled("this turn cannot be restored", theme::danger()))
        }
        PlanState::Idle => Line::raw(""),
    };

    let mut lines = vec![first];
    if let Some((text_, style)) = asked {
        lines.push(Line::from(Span::styled(text::truncate(&text_, width), style)));
    }
    lines.push(counts);
    // The overlay covers the dock, status panel included — so a refusal that
    // only wrote to `app.status` used to be invisible on the one screen it was
    // about. It belongs directly under the counts, where the eye already is.
    lines.extend(status_block(app, width));
    lines.push(rule(width));
    lines
}

fn draw_plan_body(frame: &mut Frame, area: Rect, app: &App, plan: &PlanView) {
    if plan.is_noop() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                Line::from(Span::styled(
                    " every tracked file already matches turn #".to_string()
                        + &plan.seq.to_string(),
                    theme::dim(),
                )),
                Line::from(Span::styled(" there is nothing for a restore to do.", theme::dim())),
            ]),
            area,
        );
        return;
    }

    if app.show_patch && area.width >= 88 {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)])
                .areas(area);
        draw_file_list(frame, left, app, plan);
        draw_patch(frame, right, app);
    } else if app.show_patch {
        draw_patch(frame, area, app);
    } else {
        draw_file_list(frame, area, app, plan);
    }
}

fn draw_file_list(frame: &mut Frame, area: Rect, app: &App, plan: &PlanView) {
    let width = area.width.saturating_sub(1) as usize;
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_row = 0usize;

    let section = |items: &mut Vec<ListItem>, label: &str, count: usize, style: Style| {
        items.push(ListItem::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(format!("{label} ({count})"), style),
        ])));
    };
    if plan.written > 0 {
        section(&mut items, "will be written", plan.written, theme::added());
    }
    for (index, (action, path)) in plan.files.iter().enumerate() {
        if *action == Action::Remove && index == plan.written {
            section(&mut items, "will be removed", plan.removed, theme::removed());
        }
        if index == app.plan_sel {
            selected_row = items.len();
        }
        let (marker, style) = match action {
            Action::Write => ("+", theme::added()),
            Action::Remove => ("−", theme::removed()),
        };
        let on = index == app.plan_sel;
        items.push(ListItem::new(Line::from(vec![
            Span::styled(if on { theme::CURSOR } else { theme::GUTTER }, theme::cursor_style()),
            Span::styled(format!("{marker} "), style),
            Span::styled(
                truncate_left(path, width.saturating_sub(2)),
                if on { theme::strong() } else { theme::plain() },
            ),
        ])));
    }

    let mut state = ListState::default().with_selected(Some(selected_row));
    frame.render_stateful_widget(List::new(items), area, &mut state);
}

fn draw_patch(frame: &mut Frame, area: Rect, app: &App) {
    let (title, lines): (String, Vec<Line<'static>>) = match &app.patch {
        PatchState::Ready { path, body } => {
            (path.clone(), body.lines().map(|l| Line::from(patch_span(l))).collect())
        }
        PatchState::Loading(path) => (
            path.clone(),
            vec![Line::from(Span::styled(format!(" {} reading…", spinner(app)), theme::dim()))],
        ),
        PatchState::Failed { path, message } => (
            path.clone(),
            text::wrap(message, area.width.saturating_sub(3) as usize)
                .into_iter()
                .map(|c| Line::from(Span::styled(c, theme::danger())))
                .collect(),
        ),
        PatchState::Idle => (
            "diff".into(),
            vec![Line::from(Span::styled(
                " select a file — this pane shows what the restore would change in it",
                theme::dim(),
            ))],
        ),
    };

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::dim())
        .title(Span::styled(
            format!(" {} ", truncate_left(&title, area.width.saturating_sub(4) as usize)),
            theme::accent(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines).scroll((app.patch_scroll, 0)), inner);
}

fn patch_span(line: &str) -> Span<'static> {
    let style = if line.starts_with("@@") {
        theme::accent()
    } else if line.starts_with("+++")
        || line.starts_with("---")
        || line.starts_with("diff ")
        || line.starts_with("index ")
        || line.starts_with("new file")
        || line.starts_with("deleted file")
        || line.starts_with("similarity")
    {
        theme::dim()
    } else if line.starts_with('+') {
        theme::added()
    } else if line.starts_with('-') {
        theme::removed()
    } else {
        theme::plain()
    };
    Span::styled(line.to_string(), style)
}

/// The sentence that has to be true. Everything above it is evidence; this is
/// the part that says what the next keystroke does.
fn footer(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![rule(width)];
    let say = |text: String, style: Style, lines: &mut Vec<Line<'static>>| {
        for chunk in self::text::wrap(&text, width.saturating_sub(1)) {
            lines.push(Line::from(Span::styled(format!(" {chunk}"), style)));
        }
    };

    if app.restoring {
        say(
            format!("{} checkpointing the tree you have, then restoring…", spinner(app)),
            theme::accent_strong(),
            &mut lines,
        );
        if app.quit {
            say(
                "finishing this before quitting — killing the window now would leave a tree \
                 that is neither state."
                    .to_string(),
                theme::danger(),
                &mut lines,
            );
        } else {
            say("keys are ignored until it finishes.".to_string(), theme::dim(), &mut lines);
        }
        return lines;
    }

    match &app.plan {
        PlanState::Ready(plan) if !plan.is_noop() => {
            say(
                format!(
                    "restoring rewrites {} and deletes {} under {}/.",
                    files(plan.written),
                    files(plan.removed),
                    app.repo
                ),
                theme::strong(),
                &mut lines,
            );
            say(
                "the tree you have now is snapshotted first as a new turn, so this is undoable."
                    .to_string(),
                theme::dim(),
                &mut lines,
            );
            // The last sentence before a write has to be true. A timeline can
            // carry a pane id from an earlier herdr session while this process
            // has no socket to reach it through, so the promise is only made
            // when all three of notify, a live session and a pane are there.
            let pane = app.agent_pane();
            let (notice, style) = match (app.notify, app.inside_herdr, &pane) {
                (false, _, _) => (
                    "the agent will NOT be told what changed (n turns this back on).".to_string(),
                    theme::warn(),
                ),
                (true, false, _) => (
                    "not running inside herdr — there is no agent to tell.".to_string(),
                    theme::warn(),
                ),
                (true, true, Some(pane)) => (
                    format!("the agent in pane {pane} will be told what was taken back."),
                    theme::accent(),
                ),
                (true, true, None) => (
                    "no agent pane recorded on this timeline — nobody will be told.".to_string(),
                    theme::warn(),
                ),
            };
            say(notice, style, &mut lines);

            let mut keys = vec![
                Span::styled(" shift+R ", theme::badge()),
                Span::styled(" restore   ", theme::strong()),
            ];
            keys.extend(
                hints(
                    width.saturating_sub(20),
                    &[
                        ("esc", "back"),
                        ("d", "diff"),
                        ("J/K", "scroll"),
                        ("n", "notify"),
                        ("q", "quit"),
                    ],
                )
                .spans,
            );
            lines.push(Line::from(keys));
        }
        _ => {
            lines.push(hints(width, &[("esc", "back"), ("j/k", "move"), ("q", "quit")]));
        }
    }
    lines
}

// --------------------------------------------------------------------- help

fn draw_help(frame: &mut Frame, area: Rect) {
    // Wide enough that what shows around it reads as background rather than as
    // a one-column rendering fault; full width when there is no room for that.
    let width = if area.width < 56 { area.width } else { area.width.saturating_sub(12).min(62) };
    let height = area.height.saturating_sub(2).min(20);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::accent())
        .title(Span::styled(" keys ", theme::accent_strong()));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (key, what) in [
        ("j k ↑ ↓", "move"),
        ("g G", "first / last turn"),
        ("enter", "open the rewind plan"),
        ("d", "show the patch for a file"),
        ("J K", "scroll the patch"),
        ("shift+R", "restore — only from a plan on screen"),
        ("n", "tell the agent, or don't"),
        ("r", "refresh"),
        ("esc", "back"),
        ("q", "quit"),
    ] {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(pad(key, 9), theme::accent()),
            Span::styled(what.to_string(), theme::plain()),
        ]));
    }
    lines.push(Line::raw(""));
    for chunk in text::wrap(
        "Sheep never restores from anything but a plan you have seen. The dry run is the product.",
        inner.width.saturating_sub(2) as usize,
    ) {
        lines.push(Line::from(Span::styled(format!(" {chunk}"), theme::dim())));
    }
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Left), inner);
}

// -------------------------------------------------------------------- fatal

fn draw_fatal(frame: &mut Frame, area: Rect, fatal: &Fatal) {
    let width = area.width.saturating_sub(2) as usize;
    let mut lines = vec![
        Line::from(Span::styled(" sheep ", theme::badge())),
        Line::raw(""),
        Line::from(Span::styled(format!(" {}", fatal.headline), theme::danger())),
        Line::raw(""),
    ];
    for chunk in text::wrap(&fatal.detail, width) {
        lines.push(Line::from(Span::styled(format!(" {chunk}"), theme::plain())));
    }
    if !fatal.remedy.is_empty() {
        lines.push(Line::raw(""));
        for step in &fatal.remedy {
            for chunk in text::wrap(step, width.saturating_sub(2)) {
                lines.push(Line::from(Span::styled(format!("   {chunk}"), theme::accent())));
            }
        }
    }
    lines.push(Line::raw(""));
    lines.push(hints(width, &[("q", "quit")]));
    frame.render_widget(Paragraph::new(lines), area);
}
