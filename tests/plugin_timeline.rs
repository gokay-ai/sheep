//! The recorder and the plugin's panes must name the same timeline.
//!
//! They are two programs in two languages that never talk to each other: `sheep
//! watch` files a turn under whatever `--line-by` resolves to, and a dock pane
//! reads whatever `SHEEP_LINE` says — a string `herdr-plugin/scripts/common.sh`
//! computed, out of band, from herdr's invocation context. Nothing at run time
//! notices when the two disagree. What the user sees instead is an empty
//! timeline and the words "nothing recorded yet", which is the most damaging
//! sentence this plugin can print, because it is indistinguishable from the
//! truth.
//!
//! So the agreement is asserted here, by running both halves for real: the
//! shell function in a shell, `WatchArgs` through clap, and the two answers
//! compared as strings and as turn-log paths.
//!
//! Unix only, and not as an oversight: the plugin ships for Linux and macOS
//! alone, its scripts are `bash`, and there is nothing on Windows for these
//! assertions to be about.
#![cfg(unix)]

use sheep::herdr::{LineBy, WatchArgs};
use sheep::store::Store;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A herdr pane id, in herdr 0.8's own shape: the colon is why `store::slug`
/// exists, and why a pane id is a bad timeline name.
const PANE: &str = "w31:pW";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn plugin_dir() -> PathBuf {
    repo_root().join("herdr-plugin")
}

/// What `sheep watch` with no flags at all — which is how the plugin starts it —
/// files a pane's turns under.
fn recorder_line(agent: Option<&str>) -> String {
    #[derive(clap::Parser)]
    struct Only {
        #[command(flatten)]
        watch: WatchArgs,
    }
    let args = <Only as clap::Parser>::parse_from(["sheep-watch"]).watch;
    args.line_by.timeline(PANE, agent)
}

/// Run `common.sh`'s `sheep_target_line` the way a herdr action runs it.
///
/// `env_clear` on purpose: the plugin's hooks get a curated environment, and a
/// test that passed only because the developer's own `HERDR_PANE_ID` leaked in
/// would assert nothing.
fn plugin_line(context_json: &str, path: &str) -> String {
    let common = plugin_dir().join("scripts").join("common.sh");
    let script = format!(". {} && sheep_target_line", shell_quote(&common));
    let out = Command::new(bash())
        .arg("-c")
        .arg(&script)
        .env_clear()
        .env("PATH", path)
        .env("HERDR_ENV", "1")
        .env("HERDR_WORKSPACE_ID", "w31")
        .env("HERDR_PANE_ID", PANE)
        .env("HERDR_PLUGIN_CONTEXT_JSON", context_json)
        .output()
        .expect("bash should be able to run with a cleared environment");
    assert!(
        out.status.success(),
        "sheep_target_line failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// An absolute `bash`, because the environment the function runs in is cleared
/// and there is then no PATH left to find it on.
fn bash() -> PathBuf {
    ["/bin/bash", "/usr/bin/bash", "/opt/homebrew/bin/bash"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .expect("bash is required on a platform this plugin claims to support")
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// The invocation context herdr hands an action, in the shape herdr 0.8 writes
/// it: `PluginInvocationContext` serialised as a flat JSON object.
fn context(agent: Option<&str>) -> String {
    match agent {
        Some(agent) => format!(
            r#"{{"workspace_id":"w31","workspace_cwd":"/w","focused_pane_id":"{PANE}","focused_pane_cwd":"/w","focused_pane_agent":"{agent}","focused_pane_status":"idle","invocation_source":"action"}}"#
        ),
        // herdr omits the field rather than nulling it when the pane has no
        // agent, and `jq`'s `// empty` and the sed fallback both have to cope.
        None => format!(
            r#"{{"workspace_id":"w31","workspace_cwd":"/w","focused_pane_id":"{PANE}","focused_pane_cwd":"/w","focused_pane_status":"idle","invocation_source":"action"}}"#
        ),
    }
}

/// A PATH with `sed` and `head` on it and demonstrably no `jq`, so the fallback
/// branch of `sheep_context_field` is the one that runs.
fn path_without_jq(dir: &Path) -> String {
    for tool in ["sed", "head"] {
        let source = ["/usr/bin", "/bin"]
            .iter()
            .map(|d| Path::new(d).join(tool))
            .find(|p| p.exists())
            .unwrap_or_else(|| panic!("no {tool} in /usr/bin or /bin"));
        std::os::unix::fs::symlink(source, dir.join(tool)).unwrap();
    }
    dir.display().to_string()
}

fn inherited_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
}

#[test]
fn the_plugin_and_the_recorder_resolve_the_same_timeline() {
    let expected = recorder_line(Some("claude"));
    assert_eq!(expected, "claude", "the recorder files an agent pane's turns under the agent");

    let with_jq = plugin_line(&context(Some("claude")), &inherited_path());
    assert_eq!(
        with_jq, expected,
        "the dock would read `{with_jq}` while the recorder writes `{expected}`"
    );

    // The same answer without jq installed: `sheep_context_field` falls back to
    // sed, and a fallback that returns a different string is the same bug with
    // a narrower blast radius.
    let bin = tempfile::tempdir().unwrap();
    let without_jq = plugin_line(&context(Some("claude")), &path_without_jq(bin.path()));
    assert_eq!(without_jq, expected, "the jq-less fallback must resolve the same timeline");
}

/// The string is the easy half. What has to match is the file the turns land in
/// and the ref the shadow repository updates, and both go through
/// `store::slug`.
#[test]
fn the_two_halves_land_in_the_same_turn_log() {
    let state = tempfile::tempdir().unwrap();
    let recorder = Store::open(state.path(), "wt-1", &recorder_line(Some("codex"))).unwrap();
    let plugin =
        Store::open(state.path(), "wt-1", &plugin_line(&context(Some("codex")), &inherited_path()))
            .unwrap();
    assert_eq!(recorder.path(), plugin.path());
}

/// A pane herdr attributes no agent to has no recorded turns at all — the
/// recorder skips it — so the plugin must not invent a timeline name for it.
/// `default` is the one the CLI already uses standing alone, so a manual
/// `sheep snap` from the action lands somewhere a bare `sheep log` can find.
#[test]
fn a_pane_with_no_agent_falls_back_to_the_standalone_timeline() {
    assert_eq!(plugin_line(&context(None), &inherited_path()), "default");
    let bin = tempfile::tempdir().unwrap();
    assert_eq!(plugin_line(&context(None), &path_without_jq(bin.path())), "default");
}

/// The pane id must not be the answer, with or without an agent. It is the
/// thing the two halves used to disagree about, and it is reassigned on every
/// herdr restart, so a timeline named after one is empty again by morning.
#[test]
fn the_pane_id_is_never_the_timeline() {
    for agent in [Some("claude"), None] {
        let line = plugin_line(&context(agent), &inherited_path());
        assert_ne!(line, PANE);
        assert_ne!(sheep::store::slug(&line), sheep::store::slug(PANE));
    }
}

/// The launch path, asserted rather than assumed.
///
/// The test above only means anything if the manifest really does start the
/// recorder through `watchd.sh` with no `--line-by` of its own — the moment one
/// appears anywhere under `herdr-plugin/`, the recorder stops answering to
/// `WatchArgs`' default and the agreement above is about the wrong default.
#[test]
fn nothing_in_the_plugin_overrides_line_by() {
    let manifest = std::fs::read_to_string(plugin_dir().join("herdr-plugin.toml")).unwrap();
    let launchers: Vec<&str> =
        manifest.lines().filter(|l| l.contains("command = ") && l.contains("watchd")).collect();
    assert!(
        !launchers.is_empty(),
        "the manifest no longer starts the recorder through watchd — this test is checking the wrong file"
    );

    let default = <LineBy as clap::ValueEnum>::to_possible_value(&LineBy::Agent)
        .expect("LineBy::Agent is a value clap can name")
        .get_name()
        .to_string();

    for (path, body) in plugin_files() {
        // Comments are where the flag is explained, so they are not a use of
        // it. Every file under `herdr-plugin/` comments with `#`.
        for line in body.lines().filter(|l| !l.trim_start().starts_with('#')) {
            let Some(at) = line.find("--line-by") else { continue };
            // Not forbidden — but if the plugin ever does pass it, it has to
            // pass the one `WatchArgs` would have chosen anyway.
            let value = line[at + "--line-by".len()..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches(|c: char| !c.is_ascii_alphanumeric());
            assert_eq!(
                value,
                default,
                "{} passes `--line-by {value}`, which is not the default its own panes assume",
                path.display()
            );
        }
    }
}

/// Every file the plugin ships, as (path, contents).
fn plugin_files() -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![plugin_dir()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(body) = std::fs::read_to_string(&path) {
                out.push((path, body));
            }
        }
    }
    assert!(!out.is_empty(), "no plugin files found under {}", plugin_dir().display());
    out
}
