//! Turning values into the short strings a narrow dock can show.
//!
//! Everything here is pure and width-aware. A dock is often 40 columns wide
//! next to an agent pane, so every helper takes the space it is allowed and
//! never returns more than that.
//!
//! Widths are counted in `char`s. Sheep has no unicode-width dependency (and
//! must not grow one — the binary has to cross-compile without a C toolchain),
//! so the glyphs the interface uses are restricted to ones that occupy a single
//! cell in a terminal that is not in CJK-ambiguous mode.

/// Number of terminal cells `s` is expected to occupy.
pub fn width(s: &str) -> usize {
    s.chars().count()
}

/// `s` cut to at most `max` cells, with an ellipsis when something was lost.
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if width(s) <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".into();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Collapse a screen-scraped string into one printable line.
///
/// Prompts are read off a terminal pane, so they arrive with newlines, tabs and
/// the occasional control byte. None of that can be allowed into a `Line`.
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true; // leading whitespace is dropped
    for ch in s.chars() {
        if ch.is_whitespace() || ch.is_control() {
            if !space {
                out.push(' ');
                space = true;
            }
        } else {
            out.push(ch);
            space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// "2m ago". The dock's primary answer to "when did this happen".
pub fn age(now: u64, then: u64) -> String {
    let d = now.saturating_sub(then);
    match d {
        0..=4 => "just now".into(),
        5..=59 => format!("{d}s ago"),
        60..=3_599 => format!("{}m ago", d / 60),
        3_600..=86_399 => format!("{}h ago", d / 3_600),
        86_400..=604_799 => format!("{}d ago", d / 86_400),
        _ => format!("{}w ago", d / 604_800),
    }
}

/// The same age with the "ago" dropped, for when the column is tight.
pub fn age_short(now: u64, then: u64) -> String {
    let d = now.saturating_sub(then);
    match d {
        0..=4 => "now".into(),
        5..=59 => format!("{d}s"),
        60..=3_599 => format!("{}m", d / 60),
        3_600..=86_399 => format!("{}h", d / 3_600),
        86_400..=604_799 => format!("{}d", d / 86_400),
        _ => format!("{}w", d / 604_800),
    }
}

/// `2026-08-25 14:32 UTC`.
///
/// UTC rather than local time because Sheep has no timezone database and will
/// not grow one for a caption; saying which zone it is keeps it honest.
pub fn stamp(at: u64) -> String {
    let (y, m, d) = civil_from_days((at / 86_400) as i64);
    let secs = at % 86_400;
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", secs / 3_600, (secs % 3_600) / 60)
}

/// Days since 1970-01-01 to a calendar date. Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// How many blocks of a `slots`-wide magnitude bar this turn earns, split
/// between insertions and deletions.
///
/// Scaled against the busiest turn on the timeline so the bar answers "how big
/// was this change, for this agent, today" rather than an absolute quantity
/// nobody has a feel for.
pub fn magnitude(insertions: u64, deletions: u64, max_total: u64, slots: usize) -> (usize, usize) {
    let total = insertions + deletions;
    if total == 0 || slots == 0 || max_total == 0 {
        return (0, 0);
    }
    // Always at least one block: a one-line change must not look like nothing.
    let filled =
        (((total as f64 / max_total as f64) * slots as f64).round() as usize).clamp(1, slots);
    let adds = ((insertions as f64 / total as f64) * filled as f64).round() as usize;
    let adds = adds.min(filled);
    let dels = filled - adds;
    // A change that deleted something must show at least one red block, and
    // vice versa, or the bar lies about the shape of the turn.
    match (insertions, deletions) {
        (0, _) => (0, filled),
        (_, 0) => (filled, 0),
        _ if adds == 0 => (1, filled - 1),
        _ if dels == 0 && filled > 1 => (filled - 1, 1),
        _ => (adds, dels),
    }
}

/// Greedy word wrap. Words longer than `width` are split rather than allowed to
/// overflow the panel they were measured for.
pub fn wrap(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in s.split_whitespace() {
        let mut word = word.to_string();
        while self::width(&word) > width {
            let head: String = word.chars().take(width).collect();
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            lines.push(head);
            word = word.chars().skip(width).collect();
        }
        if current.is_empty() {
            current = word;
        } else if self::width(&current) + 1 + self::width(&word) <= width {
            current.push(' ');
            current.push_str(&word);
        } else {
            lines.push(std::mem::replace(&mut current, word));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// The last component of a path, for a title that has to fit.
pub fn basename(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}
