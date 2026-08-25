//! Prompt scraping.
//!
//! `Turn.prompt` is screen-scraped and the code that fills it knows it. These
//! cases are the ones that decide whether it is worth having at all: the input
//! box of every agent looks a lot like a submitted prompt, and capturing a
//! placeholder or an empty frame would put noise on the timeline under a
//! heading that reads like fact.

use sheep::herdr::prompt::scrape;

/// Claude Code: submitted messages are echoed into the transcript with the
/// same marker the (now empty) input box uses.
const CLAUDE: &str = "\
❯ add a test for the retry path

⏺ Checking the retry helper
  ⎿  $ rg -n 'retry' src/

✻ Blanching… (3m 42s · ↓ 14.4k tokens)

────────────────────────────────────────────────
❯
────────────────────────────────────────────────
  ~/Documents/Development/Projects/sheep  main  Opus 5  ctx:10%
  ⏵⏵ bypass permissions on
";

/// Grok: the input box is a drawn frame around placeholder text.
const GROK: &str = "\
│   Özet: docs/APP_STORE_CONNECT.md
└
    Worked for 24s

 ╭──────────────────────────────────────────────╮
 │ ❯ Build anything                             │
 ╰──────────────────────────────────────────────╯
 Ctrl+\\:dashboard  │  Ctrl+x:stop
";

#[test]
fn the_submitted_prompt_is_picked_out_of_a_transcript() {
    assert_eq!(scrape(CLAUDE).as_deref(), Some("add a test for the retry path"));
}

#[test]
fn an_empty_input_box_is_not_a_prompt() {
    // The bare `❯` below the transcript is the cursor line, not something the
    // user said.
    assert_eq!(scrape("❯\n"), None);
    assert_eq!(scrape("  ❯   \n"), None);
}

#[test]
fn a_framed_placeholder_is_not_a_prompt() {
    assert_eq!(scrape(GROK), None, "a drawn input box must never become a turn's prompt");
}

#[test]
fn a_placeholder_without_a_frame_is_still_refused() {
    assert_eq!(scrape("> Ask anything\n"), None);
    assert_eq!(scrape("❯ Try \"fix the build\"\n"), None);
}

#[test]
fn the_most_recent_prompt_wins() {
    let screen = "❯ first thing\n\n⏺ done\n\n❯ second thing\n\n⏺ working\n";
    assert_eq!(scrape(screen).as_deref(), Some("second thing"));
}

#[test]
fn quoted_text_and_redirection_are_not_prompts() {
    // A diff, a shell transcript and a doctest all put `>` at the start of a
    // line without a user having typed anything.
    assert_eq!(scrape(">>> import sheep\n"), None);
    assert_eq!(scrape(">out.txt\n"), None);
    assert_eq!(scrape("> \n"), None);
    assert_eq!(scrape("| > |\n"), None);
}

#[test]
fn wrapped_padding_is_collapsed() {
    let screen = "❯   rewrite    the   parser        \n";
    assert_eq!(scrape(screen).as_deref(), Some("rewrite the parser"));
}

#[test]
fn a_pasted_wall_of_text_is_clipped() {
    let long = "x".repeat(400);
    let captured = scrape(&format!("❯ {long}\n")).expect("a long prompt is still a prompt");
    assert!(captured.chars().count() <= 161, "clipped, not stored whole: {}", captured.len());
    assert!(captured.ends_with('…'), "and it says it was clipped: {captured}");
}

#[test]
fn a_screen_with_no_prompt_on_it_yields_nothing() {
    assert_eq!(scrape(""), None);
    assert_eq!(scrape("⏺ Ran 2 shell commands\n\n  Read 1 file\n"), None);
}
