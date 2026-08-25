//! Best-effort prompt capture.
//!
//! There is no API that tells a plugin what the user typed at an agent. All
//! that exists is the pane's screen, so this is screen-scraping and it is
//! wrong sometimes: the prompt may have scrolled away, the agent may not echo
//! it, another agent's frame may look like a prompt marker. Everything it
//! returns lands in `Turn.prompt`, which is documented as not authoritative and
//! is labelled as such wherever it is shown.
//!
//! The rule is deliberately narrow, because a wrong prompt on the right turn is
//! more confusing than no prompt at all: take the last line of the visible
//! screen that starts with a prompt marker and carries real text, and refuse
//! anything that looks like an input box or its placeholder.

/// Markers agents use to echo a submitted user message.
const MARKERS: [char; 4] = ['>', '❯', '›', '»'];

/// Box-drawing characters. A marker behind one of these is a frame, not a
/// transcript line — grok draws its input box that way.
const FRAME: [char; 8] = ['│', '┃', '╎', '┆', '┊', '║', '▌', '▏'];

/// Placeholder text agents put inside an empty input box.
const PLACEHOLDERS: [&str; 6] = [
    "build anything",
    "ask anything",
    "try \"",
    "type a message",
    "how can i help",
    "what would you like",
];

/// Longest prompt we keep. Enough to recognise a turn, short enough that a
/// pasted file does not become the timeline.
const MAX: usize = 160;

/// The last thing on `screen` that looks like a submitted user prompt.
pub fn scrape(screen: &str) -> Option<String> {
    screen.lines().rev().find_map(candidate)
}

fn candidate(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let mut chars = trimmed.chars();
    let first = chars.next()?;
    if FRAME.contains(&first) || !MARKERS.contains(&first) {
        return None;
    }
    // A marker has to be followed by space: `>>= ` in a REPL transcript and
    // `>>>` in a quoted diff are not prompts.
    if !chars.next().is_some_and(char::is_whitespace) {
        return None;
    }

    let body = collapse(trimmed[first.len_utf8()..].trim());
    if body.is_empty() || !body.chars().any(char::is_alphanumeric) {
        return None;
    }
    let lowered = body.to_lowercase();
    if PLACEHOLDERS.iter().any(|p| lowered.starts_with(p)) {
        return None;
    }
    Some(clip(&body))
}

/// Terminal lines are padded and often wrapped; runs of whitespace carry no
/// meaning once they are off the screen.
fn collapse(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
            }
            space = false;
            out.push(ch);
        }
    }
    out
}

fn clip(text: &str) -> String {
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX).collect();
    out.push('…');
    out
}
