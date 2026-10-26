//! Loading a session: parse the launch lines, launch them in layout order, and place
//! every saved window as the applications bring their windows up.
//!
//! All interaction with the compositor goes through `Host`, so the sequencing can be
//! tested against a fake one.

use crate::dispatch::{Dispatcher, Op};
use crate::command_detection::{resolve_desktop_command, shell_join, shell_quote};
use crate::session::{plan_adjustment, SavedClient};
use hyprland::data::{Client, Clients, FullscreenMode, Workspace};
use hyprland::shared::{Address, HyprData, HyprDataActive};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Polls per second while waiting for windows
const TICKS_PER_SECOND: u64 = 4;
/// How long to wait for a launched program to show a window before moving on
const LAUNCH_WAIT_TICKS: u64 = 20 * TICKS_PER_SECOND;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleWorkspace {
    Id(i32),
    /// Special workspace; `None` is the anonymous one
    Special(Option<String>),
}

/// The placement rules written in front of a launch line in exec.conf
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecRules {
    pub monitor: Option<i128>,
    pub workspace: Option<RuleWorkspace>,
    pub float: bool,
    pub at: Option<(i16, i16)>,
    pub size: Option<(i16, i16)>,
    pub pin: bool,
    pub fullscreen: Option<i32>,
    /// Rule words that were not recognised (ignored)
    pub unknown: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecEntry {
    pub rules: ExecRules,
    /// The bare command, with the `[rules]` prefix removed
    pub command: String,
    /// The saved windows this launch is responsible for (new-format snapshots only)
    pub group: Option<GroupInfo>,
}

/// What a launch is expected to produce
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupInfo {
    /// The recorded command, for reports
    pub label: String,
    /// Indices into `LoadPlan::saved` of the windows of this launch's process
    pub saved: Vec<usize>,
}

fn two_numbers(rule: &str, rest: &[&str]) -> Result<(i16, i16), String> {
    match rest {
        [a, b] => Ok((
            a.parse().map_err(|_| format!("bad number in rule '{rule}'"))?,
            b.parse().map_err(|_| format!("bad number in rule '{rule}'"))?,
        )),
        _ => Err(format!("rule '{rule}' needs two numbers")),
    }
}

fn parse_rules(text: &str) -> Result<ExecRules, String> {
    let mut rules = ExecRules::default();
    for rule in text.split(';').map(str::trim).filter(|r| !r.is_empty()) {
        let words: Vec<&str> = rule.split_whitespace().collect();
        let (name, rest) = (words[0], &words[1..]);
        match name {
            "monitor" => {
                rules.monitor = Some(
                    rest.first()
                        .and_then(|m| m.parse().ok())
                        .ok_or_else(|| format!("bad monitor in rule '{rule}'"))?,
                )
            }
            "workspace" => {
                let target = rest.first().ok_or_else(|| format!("rule '{rule}' needs a workspace"))?;
                rules.workspace = Some(if *target == "special" {
                    RuleWorkspace::Special(None)
                } else if let Some(name) = target.strip_prefix("special:") {
                    RuleWorkspace::Special(Some(name.to_string()))
                } else {
                    RuleWorkspace::Id(
                        target.parse().map_err(|_| format!("bad workspace in rule '{rule}'"))?,
                    )
                });
            }
            "float" => rules.float = true,
            "pin" => rules.pin = true,
            "move" => rules.at = Some(two_numbers(rule, rest)?),
            "size" => rules.size = Some(two_numbers(rule, rest)?),
            "fullscreenstate" => {
                rules.fullscreen = Some(
                    rest.first()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| format!("bad state in rule '{rule}'"))?,
                )
            }
            _ => rules.unknown.push(rule.to_string()),
        }
    }
    Ok(rules)
}

/// Parse one exec.conf line: `[rules] command`, or a bare command
pub fn parse_exec_line(line: &str) -> Result<ExecEntry, String> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix('[') {
        // Rules never contain ']', commands might, so split at the first one
        let (rules, command) = rest
            .split_once(']')
            .ok_or_else(|| format!("unterminated rules in line: {line}"))?;
        let command = command.trim();
        if command.is_empty() {
            return Err(format!("no command in line: {line}"));
        }
        Ok(ExecEntry { rules: parse_rules(rules)?, command: command.to_string(), group: None })
    } else if line.is_empty() {
        Err("empty line".to_string())
    } else {
        Ok(ExecEntry { rules: ExecRules::default(), command: line.to_string(), group: None })
    }
}

/// Parse the whole of exec.conf; blank lines are skipped, any bad line is an error
pub fn parse_exec_conf(text: &str) -> Result<Vec<ExecEntry>, String> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(n, l)| parse_exec_line(l).map_err(|e| format!("exec.conf line {}: {e}", n + 1)))
        .collect()
}

/// Placement rules equivalent to a saved window's state
fn rules_from_client(client: &Client) -> ExecRules {
    let workspace = match client.workspace.name.strip_prefix("special:") {
        Some(name) => RuleWorkspace::Special(Some(name.to_string())),
        None if client.workspace.id == -99 => RuleWorkspace::Special(None),
        None => RuleWorkspace::Id(client.workspace.id),
    };
    ExecRules {
        monitor: client.monitor,
        workspace: Some(workspace),
        float: client.floating,
        at: Some(client.at),
        size: Some(client.size),
        pin: client.pinned,
        fullscreen: match client.fullscreen {
            FullscreenMode::None => None,
            mode => Some(mode as i32),
        },
        unknown: vec![],
    }
}

/// The command line recorded for a saved window. A single-element argv is already a
/// complete command line (Chromium-family processes present /proc cmdline as one
/// space-joined string, quotes and all), so it is used verbatim; several elements are
/// shell-joined with quoting.
fn command_from_saved_argv(argv: &[String]) -> Option<String> {
    match argv {
        [] => None,
        [line] if line.trim().is_empty() => None,
        [line] => Some(line.clone()),
        argv => Some(shell_join(argv)),
    }
}

/// Whether the recorded program is the inside view of a Flatpak sandbox (`/app/...`),
/// which does not exist on the host
fn is_sandbox_internal(argv: &[String]) -> bool {
    argv.first()
        .and_then(|first| first.split_whitespace().next())
        .map_or(false, |program| program.starts_with("/app/"))
}

/// Launch entries for a new-format snapshot, built from the recorded argv. Windows are
/// grouped by command; each distinct pid in a group is one launch (identical commands
/// in separate processes are separate launches, windows sharing a process are one),
/// with rules taken from the group's first saved window. A window whose argv is
/// sandbox-internal launches through its desktop entry instead. Windows without argv
/// cannot be launched and are skipped (they are still placed if their app brings them
/// up).
pub fn launch_entries_from_saved(saved: &[SavedClient], xdg_map: &HashMap<String, String>) -> Vec<ExecEntry> {
    launch_entries_with(saved, xdg_map, &|dir| dir.is_dir())
}

/// Prefix a launch with `cd <cwd> &&` so it runs from its recorded directory. A
/// directory that no longer exists leaves the launch unwrapped, with a warning.
pub fn wrap_cwd(command: String, cwd: Option<&str>, window: &Client, dir_exists: &dyn Fn(&Path) -> bool) -> String {
    let Some(dir) = cwd else { return command };
    if dir_exists(Path::new(dir)) {
        format!("cd {} && {}", shell_quote(dir), command)
    } else {
        println!(
            "Warning: working directory '{dir}' of '{}' ({}) no longer exists; launching from the loader's directory",
            window.title, window.class
        );
        command
    }
}

/// `launch_entries_from_saved` with the directory check injected
pub fn launch_entries_with(
    saved: &[SavedClient],
    xdg_map: &HashMap<String, String>,
    dir_exists: &dyn Fn(&Path) -> bool,
) -> Vec<ExecEntry> {
    let mut entries: Vec<ExecEntry> = vec![];
    let mut group_rules: HashMap<String, ExecRules> = HashMap::new();
    let mut launched: HashSet<(String, i32)> = HashSet::new();
    for window in saved {
        let Some(argv) = window.argv.as_deref() else { continue };
        let Some(mut command) = command_from_saved_argv(argv) else { continue };
        let sandboxed = is_sandbox_internal(argv);
        if sandboxed {
            match resolve_desktop_command(&window.client, xdg_map) {
                Some(entry_command) => command = entry_command,
                None => println!(
                    "Warning: '{}' ({}) runs inside a sandbox and has no desktop entry; launching its recorded command",
                    window.client.title, window.client.class
                ),
            }
        }
        let rules = group_rules
            .entry(command.clone())
            .or_insert_with(|| rules_from_client(&window.client))
            .clone();
        if launched.insert((command.clone(), window.client.pid)) {
            // A sandbox's cwd is a path inside the sandbox: not wrapped, not warned about
            let launch = if sandboxed {
                command
            } else {
                wrap_cwd(command, window.cwd.as_deref(), &window.client, dir_exists)
            };
            entries.push(ExecEntry { rules, command: launch, group: None });
        }
    }
    entries
}

/// Order launches so a tiling layout regrows as it was: workspace ascending, then
/// saved position (x, then y), floating windows last within each workspace
pub fn launch_order(mut entries: Vec<ExecEntry>) -> Vec<ExecEntry> {
    entries.sort_by_key(|e| {
        let workspace = match &e.rules.workspace {
            Some(RuleWorkspace::Id(id)) => *id as i64,
            Some(RuleWorkspace::Special(_)) => i64::MAX,
            None => 0,
        };
        let (x, y) = e.rules.at.unwrap_or((0, 0));
        (workspace, e.rules.float, x, y)
    });
    entries
}

/// The state a freshly launched window should end up in according to its launch rules
pub fn desired_from_rules(live: &Client, rules: &ExecRules) -> Client {
    let mut wanted = live.clone();
    match &rules.workspace {
        Some(RuleWorkspace::Id(id)) => {
            wanted.workspace.id = *id;
            wanted.workspace.name = id.to_string();
        }
        Some(RuleWorkspace::Special(None)) => {
            wanted.workspace.id = -99;
            wanted.workspace.name = "special".to_string();
        }
        Some(RuleWorkspace::Special(Some(name))) => {
            wanted.workspace.id = -98;
            wanted.workspace.name = format!("special:{name}");
        }
        None => {}
    }
    if let Some(monitor) = rules.monitor {
        wanted.monitor = Some(monitor);
    }
    wanted.floating = rules.float;
    wanted.pinned = rules.pin;
    if let Some(at) = rules.at {
        wanted.at = at;
    }
    if let Some(size) = rules.size {
        wanted.size = size;
    }
    wanted.fullscreen = match rules.fullscreen {
        Some(1) => FullscreenMode::Maximized,
        Some(2) => FullscreenMode::Fullscreen,
        Some(3) => FullscreenMode::MaximizedFullscreen,
        _ => FullscreenMode::None,
    };
    wanted
}

/// A stand-in for a window that does not exist yet (used to render simulated dispatches)
pub fn placeholder_client() -> Client {
    serde_json::from_value(serde_json::json!({
        "address": "0x<address>",
        "at": [0, 0],
        "size": [0, 0],
        "workspace": { "id": 0, "name": "0" },
        "floating": false,
        "fullscreen": 0,
        "fullscreenClient": 0,
        "monitor": 0,
        "initialClass": "",
        "class": "",
        "initialTitle": "",
        "title": "",
        "pid": 0,
        "xwayland": false,
        "pinned": false,
        "grouped": [],
        "mapped": true,
        "swallowing": null,
        "focusHistoryID": 0
    }))
    .expect("placeholder client is valid")
}

struct Target {
    saved: Client,
    matched: Option<Address>,
}

/// Matches live windows to saved clients: by class, in saved order within the class,
/// with an exact title match among a class's remaining candidates winning the tie
pub struct Placer {
    targets: Vec<Target>,
}

impl Placer {
    pub fn new(saved: Vec<Client>) -> Self {
        Placer { targets: saved.into_iter().map(|saved| Target { saved, matched: None }).collect() }
    }

    pub fn is_complete(&self) -> bool {
        self.targets.iter().all(|t| t.matched.is_some())
    }

    pub fn unmatched(&self) -> Vec<&Client> {
        self.targets.iter().filter(|t| t.matched.is_none()).map(|t| &t.saved).collect()
    }

    /// Claim a saved client for a live window; `None` when the window was already
    /// claimed or nothing saved fits it
    pub fn assign(&mut self, live: &Client) -> Option<Client> {
        if self.targets.iter().any(|t| t.matched.as_ref() == Some(&live.address)) {
            return None;
        }
        let same_class = |t: &Target| {
            t.matched.is_none()
                && (t.saved.class == live.class || t.saved.initial_class == live.initial_class)
        };
        let index = self
            .targets
            .iter()
            .position(|t| same_class(t) && t.saved.title == live.title)
            .or_else(|| self.targets.iter().position(same_class))?;
        self.targets[index].matched = Some(live.address.clone());
        Some(self.targets[index].saved.clone())
    }
}

/// Everything load needs to know, decided before anything is touched
pub struct LoadPlan {
    /// Launch lines in launch order (empty when only adjusting)
    pub entries: Vec<ExecEntry>,
    /// Saved windows to place by class (new-format snapshots, or adjust-only)
    pub saved: Vec<Client>,
    /// Place by class from `saved`; otherwise place each launch's first new window by its rules
    pub place_by_class: bool,
    /// Polls to keep placing after the last launch
    pub tail_ticks: u64,
    /// Only this workspace's slice is being loaded
    pub scope: Option<i32>,
    /// Workspace to focus once finished
    pub restore_workspace: Option<i32>,
}

impl LoadPlan {
    pub fn tail_ticks_for(load_time: u64) -> u64 {
        load_time * TICKS_PER_SECOND
    }
}

#[derive(Debug, Default)]
pub struct LoadReport {
    pub placed: usize,
    pub unmatched: Vec<Client>,
}

/// The compositor as load sees it
pub trait Host {
    fn clients(&self) -> hyprland::Result<Vec<Client>>;
    fn send(&self, op: &Op) -> hyprland::Result<()>;
    fn active_workspace(&self) -> Option<i32>;
    /// Signal a process
    fn signal(&self, pid: i32, signal: Signal);
    /// Wait one poll interval
    fn pause(&self);
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

pub struct RealHost {
    pub dispatcher: Dispatcher,
}

impl Host for RealHost {
    fn clients(&self) -> hyprland::Result<Vec<Client>> {
        Ok(Clients::get()?.iter().cloned().collect())
    }

    fn send(&self, op: &Op) -> hyprland::Result<()> {
        self.dispatcher.send(op)
    }

    fn active_workspace(&self) -> Option<i32> {
        Workspace::get_active().ok().map(|w| w.id)
    }

    fn signal(&self, pid: i32, signal: Signal) {
        let flag = match signal {
            Signal::Term => "-TERM",
            Signal::Kill => "-KILL",
        };
        let _ = std::process::Command::new("kill").arg(flag).arg(pid.to_string()).output();
    }

    fn pause(&self) {
        std::thread::sleep(std::time::Duration::from_millis(1000 / TICKS_PER_SECOND));
    }
}

/// How long each stage of clearing waits before escalating
pub struct ClearConfig {
    pub close_ticks: u64,
    pub term_ticks: u64,
    pub kill_ticks: u64,
}

impl Default for ClearConfig {
    fn default() -> Self {
        ClearConfig {
            close_ticks: 15 * TICKS_PER_SECOND,
            term_ticks: 10 * TICKS_PER_SECOND,
            kill_ticks: 5 * TICKS_PER_SECOND,
        }
    }
}

#[derive(Debug, Default)]
pub struct ClearReport {
    /// Titles of windows that went away after a graceful close
    pub closed: Vec<String>,
    pub termed: Vec<i32>,
    pub killed: Vec<i32>,
}

fn describe(clients: &[Client]) -> String {
    clients
        .iter()
        .map(|c| format!("'{}' ({}, pid {})", c.title, c.class, c.pid))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Poll until no clients remain or the ticks run out; returns what is left
fn wait_for_empty(host: &dyn Host, ticks: u64) -> Result<Vec<Client>, String> {
    let mut last = None;
    for _ in 0..=ticks {
        if let Ok(live) = host.clients() {
            if live.is_empty() {
                return Ok(live);
            }
            last = Some(live);
        }
        if last.is_some() || ticks > 0 {
            host.pause();
        }
    }
    last.ok_or_else(|| "cannot read the client list from Hyprland".to_string())
}

/// Pids worth signalling: distinct, and never init or the kernel
fn signal_targets(clients: &[Client]) -> Vec<i32> {
    let mut pids: Vec<i32> = vec![];
    for c in clients {
        if c.pid > 1 && !pids.contains(&c.pid) {
            pids.push(c.pid);
        }
    }
    pids
}

/// Close every window, escalating only for what stays: graceful close, then SIGTERM,
/// then SIGKILL, each followed by a bounded wait. Fails, naming the survivors, if
/// anything outlives SIGKILL.
pub fn clear_session(host: &dyn Host, config: &ClearConfig) -> Result<ClearReport, String> {
    let mut report = ClearReport::default();
    let initial = host.clients().map_err(|e| format!("cannot read the client list: {e}"))?;
    if initial.is_empty() {
        return Ok(report);
    }

    println!("Closing {} windows gracefully", initial.len());
    for client in &initial {
        if host.send(&Op::Close(client.address.clone())).is_err() {
            println!("Warning: close request failed for '{}'", client.title);
        }
    }
    let mut left = wait_for_empty(host, config.close_ticks)?;
    let still_open: HashSet<&Address> = left.iter().map(|c| &c.address).collect();
    report.closed = initial
        .iter()
        .filter(|c| !still_open.contains(&c.address))
        .map(|c| c.title.clone())
        .collect();
    println!("Closed gracefully: {}", report.closed.len());
    for title in &report.closed {
        println!("  closed '{title}'");
    }
    if left.is_empty() {
        return Ok(report);
    }

    let pids = signal_targets(&left);
    println!("Still open after graceful close: {}", describe(&left));
    println!("Sending SIGTERM to pids {pids:?}");
    pids.iter().for_each(|&pid| host.signal(pid, Signal::Term));
    report.termed = pids;
    left = wait_for_empty(host, config.term_ticks)?;
    if left.is_empty() {
        return Ok(report);
    }

    let pids = signal_targets(&left);
    println!("Still open after SIGTERM: {}", describe(&left));
    println!("Sending SIGKILL to pids {pids:?}");
    pids.iter().for_each(|&pid| host.signal(pid, Signal::Kill));
    report.killed = pids;
    left = wait_for_empty(host, config.kill_ticks)?;
    if left.is_empty() {
        return Ok(report);
    }

    Err(format!(
        "could not clear the desktop; these windows survived SIGKILL: {}",
        describe(&left)
    ))
}

fn apply(host: &dyn Host, live: &Client, wanted: &Client) {
    println!("found '{}' on workspace: {}", live.title, live.workspace.id);
    for op in plan_adjustment(live, wanted) {
        if host.send(&op).is_err() {
            println!("Warning: Failed to apply {:?}: {}", op, live.title);
        }
    }
}

fn place_live(host: &dyn Host, placer: &mut Placer, live: &[Client], report: &mut LoadReport) {
    for window in live.iter().filter(|c| c.mapped) {
        if let Some(wanted) = placer.assign(window) {
            apply(host, window, &wanted);
            report.placed += 1;
        }
    }
}

/// Run a load: launch each entry in order waiting for its window, placing windows as
/// they appear, then keep placing until everything saved is matched or time runs out
pub fn run_load(host: &dyn Host, plan: &LoadPlan) -> LoadReport {
    let mut placer = Placer::new(if plan.place_by_class { plan.saved.clone() } else { vec![] });
    let mut report = LoadReport::default();

    for entry in &plan.entries {
        let before: HashSet<Address> = host
            .clients()
            .map(|c| c.into_iter().map(|c| c.address).collect())
            .unwrap_or_default();
        if let Err(e) = host.send(&Op::Exec(entry.command.clone())) {
            println!("Warning: failed to launch '{}': {e}", entry.command);
            continue;
        }
        for _ in 0..LAUNCH_WAIT_TICKS {
            host.pause();
            let Ok(live) = host.clients() else { continue };
            if plan.place_by_class {
                place_live(host, &mut placer, &live, &mut report);
            }
            let new_window = live.iter().find(|c| c.mapped && !before.contains(&c.address));
            if let Some(window) = new_window {
                if !plan.place_by_class {
                    apply(host, window, &desired_from_rules(window, &entry.rules));
                    report.placed += 1;
                }
                break;
            }
        }
    }

    if plan.place_by_class {
        let mut ticks = 0;
        while !placer.is_complete() && ticks < plan.tail_ticks {
            host.pause();
            ticks += 1;
            if let Ok(live) = host.clients() {
                place_live(host, &mut placer, &live, &mut report);
            }
        }
        report.unmatched = placer.unmatched().into_iter().cloned().collect();
        for client in &report.unmatched {
            println!("Warning: no window appeared for saved client '{}' ({})", client.title, client.class);
        }
    }

    if let Some(workspace) = plan.restore_workspace {
        let _ = host.send(&Op::FocusWorkspace(workspace));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::client;
    use std::cell::{Cell, RefCell};

    #[test]
    fn test_parse_exec_line_rules_and_command() {
        let e = parse_exec_line(
            "[monitor 1;workspace 3 silent;float;move 10 20;size 800 600;pin;fullscreenstate 2] kitty --title 'a b'",
        ).unwrap();
        assert_eq!(e.command, "kitty --title 'a b'");
        assert_eq!(e.rules.monitor, Some(1));
        assert_eq!(e.rules.workspace, Some(RuleWorkspace::Id(3)));
        assert!(e.rules.float && e.rules.pin);
        assert_eq!(e.rules.at, Some((10, 20)));
        assert_eq!(e.rules.size, Some((800, 600)));
        assert_eq!(e.rules.fullscreen, Some(2));
    }

    #[test]
    fn test_parse_exec_line_edge_cases() {
        // Brackets inside the command are not mistaken for the end of the rules
        assert_eq!(parse_exec_line("[workspace 1 silent] sh -c 'echo [x]'").unwrap().command, "sh -c 'echo [x]'");
        assert_eq!(parse_exec_line("[] true").unwrap().rules, ExecRules::default());
        assert_eq!(parse_exec_line("true").unwrap().command, "true");
        assert_eq!(
            parse_exec_line("[workspace special silent] x").unwrap().rules.workspace,
            Some(RuleWorkspace::Special(None))
        );
        assert_eq!(
            parse_exec_line("[workspace special:scratch silent] x").unwrap().rules.workspace,
            Some(RuleWorkspace::Special(Some("scratch".to_string())))
        );
        assert_eq!(parse_exec_line("[someday 4] x").unwrap().rules.unknown, vec!["someday 4"]);
    }

    #[test]
    fn test_parse_exec_line_rejects_malformed() {
        assert!(parse_exec_line("[monitor ;workspace silent;move size] unknown_command").is_err());
        assert!(parse_exec_line("[workspace 1 silent kitty").is_err());
        assert!(parse_exec_line("[workspace 1 silent]").is_err());
        assert!(parse_exec_line("[move 1] x").is_err());
        let err = parse_exec_conf("[workspace 1 silent] ok\n\n[move a b] bad\n").unwrap_err();
        assert!(err.contains("line 3"), "{err}");
    }

    fn saved(class: &str, argv: Option<&[&str]>, ws: i32, floating: bool, at: (i16, i16)) -> SavedClient {
        SavedClient {
            client: client(class, "t", ws, floating, at, (100, 50)),
            argv: argv.map(|a| a.iter().map(|s| s.to_string()).collect()),
            cwd: Some("/home/synthetic".to_string()),
            foreground: None,
        }
    }

    #[test]
    fn test_launch_entries_from_saved_argv() {
        let entries = launch_entries_from_saved_no_xdg(&[
            saved("brave", Some(&["/opt/brave/brave", "--app=https://example.test/a b"]), 5, false, (0, 0)),
            // Another window of the same process: same command, not launched again
            saved("brave", Some(&["/opt/brave/brave", "--app=https://example.test/a b"]), 6, true, (9, 9)),
            // A second instance with its own profile is a distinct launch
            saved("brave", Some(&["/opt/brave/brave", "--user-data-dir=/home/x/p"]), 1, false, (3, 4)),
            // No argv: cannot be launched
            saved("ghost", None, 2, false, (0, 0)),
        ]);
        let commands: Vec<&str> = entries.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(commands, vec![
            "/opt/brave/brave '--app=https://example.test/a b'",
            "/opt/brave/brave --user-data-dir=/home/x/p",
        ]);
        // Rules come from the first window carrying each command
        assert_eq!(entries[0].rules.workspace, Some(RuleWorkspace::Id(5)));
        assert!(!entries[0].rules.float);
        assert_eq!(entries[1].rules.at, Some((3, 4)));
    }

    fn launch_entries_from_saved_no_xdg(saved: &[SavedClient]) -> Vec<ExecEntry> {
        launch_entries_from_saved(saved, &HashMap::new())
    }

    fn saved_pid(class: &str, argv: &[&str], pid: i32, at: (i16, i16)) -> SavedClient {
        let mut window = saved(class, Some(argv), 1, false, at);
        window.client.pid = pid;
        window
    }

    #[test]
    fn test_single_element_argv_is_used_verbatim() {
        // Chromium-family cmdline: one space-joined string with shell quotes embedded
        let blob = r#"/opt/electronmail/electron-mail --class=electron-mail --name="Electron Mail" --user-data-dir='/home/x/em data'"#;
        let entries = launch_entries_from_saved_no_xdg(&[saved_pid("electron-mail", &[blob], 7, (0, 0))]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, blob);
        // Multi-element argv is still shell-joined
        let entries = launch_entries_from_saved_no_xdg(&[saved_pid("x", &["a b", "c"], 8, (0, 0))]);
        assert_eq!(entries[0].command, "'a b' c");
        // A blank single element is not a command
        assert!(launch_entries_from_saved_no_xdg(&[saved_pid("x", &["  "], 9, (0, 0))]).is_empty());
    }

    #[test]
    fn test_same_command_in_separate_processes_is_launched_per_pid() {
        let entries = launch_entries_from_saved_no_xdg(&[
            saved_pid("foot", &["foot"], 101, (0, 0)),
            saved_pid("foot", &["foot"], 102, (10, 0)),
            saved_pid("foot", &["foot"], 103, (20, 0)),
        ]);
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|e| e.command == "foot"));
        // Rules come from the group's first window
        assert!(entries.iter().all(|e| e.rules.at == Some((0, 0))));
    }

    #[test]
    fn test_two_windows_of_one_pid_are_one_launch() {
        let entries = launch_entries_from_saved_no_xdg(&[
            saved_pid("brave", &["brave --profile-directory=Default"], 55, (0, 0)),
            saved_pid("brave", &["brave --profile-directory=Default"], 55, (30, 0)),
            saved_pid("brave", &["brave --profile-directory=Default"], 56, (60, 0)),
        ]);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_sandbox_internal_argv_launches_through_the_desktop_entry() {
        let mut xdg = HashMap::new();
        xdg.insert(
            crate::command_faker::xdg_id_key("dev.vencord.vesktop"),
            "/usr/bin/flatpak run --command=vesktop dev.vencord.Vesktop".to_string(),
        );
        let mut vesktop = saved_pid("dev.vencord.Vesktop", &["/app/bin/vesktop/vesktop.bin --no-sandbox"], 40, (0, 0));
        vesktop.client.initial_class = "dev.vencord.Vesktop".to_string();
        let mut vesktop2 = vesktop.clone();
        vesktop2.client.pid = 41;
        let host_app = saved_pid("kitty", &["/usr/bin/kitty --single-instance"], 42, (0, 0));
        let entries = launch_entries_from_saved(&[vesktop.clone(), vesktop2, host_app], &xdg);
        let commands: Vec<&str> = entries.iter().map(|e| e.command.as_str()).collect();
        // The entry command replaces the blob; multiplicity is still per pid; the host app is untouched
        assert_eq!(commands, vec![
            "/usr/bin/flatpak run --command=vesktop dev.vencord.Vesktop",
            "/usr/bin/flatpak run --command=vesktop dev.vencord.Vesktop",
            "/usr/bin/kitty --single-instance",
        ]);
        // Multi-element sandbox argv is recognised too
        let multi = saved_pid("dev.vencord.Vesktop", &["/app/bin/vesktop", "--x"], 43, (0, 0));
        let entries = launch_entries_from_saved(&[multi], &xdg);
        assert!(entries[0].command.starts_with("/usr/bin/flatpak run"));
        // With no desktop entry the recorded command is kept
        let entries = launch_entries_from_saved(&[vesktop], &HashMap::new());
        assert_eq!(entries[0].command, "/app/bin/vesktop/vesktop.bin --no-sandbox");
    }

    fn saved_cwd(argv: &[&str], pid: i32, cwd: Option<&str>) -> SavedClient {
        let mut window = saved_pid("app", argv, pid, (0, 0));
        window.cwd = cwd.map(str::to_string);
        window
    }

    #[test]
    fn test_cwd_wraps_every_launch_with_a_recorded_directory() {
        let exists = |_: &Path| true;
        let entries = launch_entries_with(
            &[
                // The playcall pattern: a relative-path launch
                saved_cwd(&["./playcall-gui"], 1, Some("/home/synthetic/Work/playcall-gui")),
                // Not only relative launches (DEC-1), and paths needing quoting
                saved_cwd(&["/usr/bin/kitty", "--single-instance"], 2, Some("/home/synthetic/my dir")),
                // Legacy: no recorded cwd launches exactly as in v1
                saved_cwd(&["mpv"], 3, None),
            ],
            &HashMap::new(),
            &exists,
        );
        let commands: Vec<&str> = entries.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(commands, vec![
            "cd /home/synthetic/Work/playcall-gui && ./playcall-gui",
            "cd '/home/synthetic/my dir' && /usr/bin/kitty --single-instance",
            "mpv",
        ]);
    }

    #[test]
    fn test_missing_cwd_launches_unwrapped() {
        let gone = |_: &Path| false;
        let entries = launch_entries_with(&[saved_cwd(&["./playcall-gui"], 1, Some("/gone"))], &HashMap::new(), &gone);
        assert_eq!(entries[0].command, "./playcall-gui");
    }

    #[test]
    fn test_sandbox_launches_skip_the_cwd_wrap_silently() {
        let mut xdg = HashMap::new();
        xdg.insert(
            crate::command_faker::xdg_id_key("dev.vencord.vesktop"),
            "/usr/bin/flatpak run dev.vencord.Vesktop".to_string(),
        );
        let mut vesktop = saved_cwd(&["/app/bin/vesktop"], 5, Some("/app/some/sandbox/dir"));
        vesktop.client.initial_class = "dev.vencord.Vesktop".to_string();
        let exists = |_: &Path| true;
        let entries = launch_entries_with(&[vesktop], &xdg, &exists);
        assert_eq!(entries[0].command, "/usr/bin/flatpak run dev.vencord.Vesktop");
    }

    #[test]
    fn test_wrapped_relative_launch_runs_from_its_directory() {
        // Real shell: a relative-path program only runs because of the cd
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let script = project.path().join("playcall-gui");
        std::fs::write(&script, "#!/bin/sh\npwd > ran-from\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let exists = |p: &Path| p.is_dir();
        let entries = launch_entries_with(
            &[saved_cwd(&["./playcall-gui"], 1, Some(project.path().to_str().unwrap()))],
            &HashMap::new(),
            &exists,
        );
        let status = std::process::Command::new("sh")
            .arg("-c").arg(&entries[0].command).current_dir(elsewhere.path()).status().unwrap();
        assert!(status.success());
        let ran_from = std::fs::read_to_string(project.path().join("ran-from")).unwrap();
        assert_eq!(std::fs::canonicalize(ran_from.trim()).unwrap(), std::fs::canonicalize(project.path()).unwrap());

        // Without the wrap the same launch fails from the loader's directory
        let bare = std::process::Command::new("sh").arg("-c").arg("./playcall-gui")
            .current_dir(elsewhere.path()).output().unwrap();
        assert!(!bare.status.success());
    }

    #[test]
    fn test_launch_order() {
        let order = |lines: &[&str]| -> Vec<String> {
            launch_order(lines.iter().map(|l| parse_exec_line(l).unwrap()).collect())
                .into_iter().map(|e| e.command).collect()
        };
        assert_eq!(
            order(&[
                "[workspace 2 silent;move 0 0] b",
                "[workspace 1 silent;float;move 0 0] float1",
                "[workspace 1 silent;move 900 0] right",
                "[workspace 1 silent;move 0 500] lower",
                "[workspace 1 silent;move 0 0] left",
                "[workspace special silent] special",
            ]),
            vec!["left", "lower", "right", "float1", "b", "special"]
        );
    }

    #[test]
    fn test_placer_matches_by_class_then_order_with_title_tiebreak() {
        let mut placer = Placer::new(vec![
            client("brave", "one", 1, false, (0, 0), (1, 1)),
            client("brave", "two", 2, false, (0, 0), (1, 1)),
            client("kitty", "term", 3, false, (0, 0), (1, 1)),
        ]);
        // A brave window titled "two" claims the "two" client even though "one" is first
        let two = client("brave", "two", 9, false, (5, 0), (1, 1));
        assert_eq!(placer.assign(&two).unwrap().workspace.id, 2);
        // Re-presenting the same window claims nothing more
        assert!(placer.assign(&two).is_none());
        // A differently titled brave window takes the remaining one, in saved order
        let other = client("brave", "changed title", 9, false, (6, 0), (1, 1));
        assert_eq!(placer.assign(&other).unwrap().workspace.id, 1);
        // A third brave window has nothing left to claim
        assert!(placer.assign(&client("brave", "x", 9, false, (7, 0), (1, 1))).is_none());
        assert!(!placer.is_complete());
        assert_eq!(placer.unmatched().len(), 1);
        assert!(placer.assign(&client("kitty", "whatever", 9, false, (8, 0), (1, 1))).is_some());
        assert!(placer.is_complete());
    }

    #[test]
    fn test_same_title_windows_are_placed_in_order() {
        let mut placer = Placer::new(vec![
            client("kitty", "zsh", 1, false, (0, 0), (1, 1)),
            client("kitty", "zsh", 2, false, (0, 0), (1, 1)),
        ]);
        assert_eq!(placer.assign(&client("kitty", "zsh", 9, false, (1, 0), (1, 1))).unwrap().workspace.id, 1);
        assert_eq!(placer.assign(&client("kitty", "zsh", 9, false, (2, 0), (1, 1))).unwrap().workspace.id, 2);
    }

    /// A compositor whose launches make scripted windows appear after a delay in ticks
    struct FakeHost {
        tick: Cell<u64>,
        windows: RefCell<Vec<(u64, Client)>>,
        scripts: RefCell<std::collections::VecDeque<Vec<(u64, Client)>>>,
        sent: RefCell<Vec<Op>>,
        pauses: Cell<u64>,
        signals: RefCell<Vec<(i32, Signal)>>,
        /// pid -> how much it takes to remove its windows: 0 close, 1 SIGTERM, 2 SIGKILL, 3 never
        resist: RefCell<std::collections::HashMap<i32, u8>>,
    }

    impl FakeHost {
        fn new(existing: Vec<Client>, scripts: Vec<Vec<(u64, Client)>>) -> Self {
            FakeHost {
                tick: Cell::new(0),
                windows: RefCell::new(existing.into_iter().map(|c| (0, c)).collect()),
                scripts: RefCell::new(scripts.into()),
                sent: RefCell::new(vec![]),
                pauses: Cell::new(0),
                signals: RefCell::new(vec![]),
                resist: RefCell::new(Default::default()),
            }
        }
    }

    impl Host for FakeHost {
        fn clients(&self) -> hyprland::Result<Vec<Client>> {
            Ok(self.windows.borrow().iter().filter(|(due, _)| *due <= self.tick.get()).map(|(_, c)| c.clone()).collect())
        }
        fn send(&self, op: &Op) -> hyprland::Result<()> {
            if let Op::Exec(_) = op {
                for (delay, c) in self.scripts.borrow_mut().pop_front().unwrap_or_default() {
                    self.windows.borrow_mut().push((self.tick.get() + delay, c));
                }
            }
            if let Op::Close(addr) = op {
                let resist = self.resist.borrow().clone();
                self.windows.borrow_mut().retain(|(_, c)| {
                    !(c.address == *addr && resist.get(&c.pid).copied().unwrap_or(0) == 0)
                });
            }
            self.sent.borrow_mut().push(op.clone());
            Ok(())
        }
        fn signal(&self, pid: i32, signal: Signal) {
            self.signals.borrow_mut().push((pid, signal));
            let needed = match signal { Signal::Term => 1, Signal::Kill => 2 };
            let resist = self.resist.borrow().get(&pid).copied().unwrap_or(0);
            if resist <= needed {
                self.windows.borrow_mut().retain(|(_, c)| c.pid != pid);
            }
        }
        fn active_workspace(&self) -> Option<i32> { Some(1) }
        fn pause(&self) {
            self.tick.set(self.tick.get() + 1);
            self.pauses.set(self.pauses.get() + 1);
        }
    }

    fn entry(line: &str) -> ExecEntry { parse_exec_line(line).unwrap() }

    fn execs(host: &FakeHost) -> Vec<String> {
        host.sent.borrow().iter().filter_map(|o| match o { Op::Exec(c) => Some(c.clone()), _ => None }).collect()
    }

    #[test]
    fn test_old_format_places_each_launchs_first_new_window_by_rules() {
        let a = client("kitty", "a", 1, false, (0, 0), (50, 50));
        let b = client("mpv", "b", 1, false, (3, 0), (50, 50));
        let host = FakeHost::new(vec![], vec![vec![(2, a.clone())], vec![(3, b.clone())]]);
        let plan = LoadPlan {
            entries: launch_order(vec![
                entry("[workspace 4 silent;float;move 100 200;size 300 400] mpv"),
                entry("[workspace 2 silent;move 0 0] kitty"),
            ]),
            saved: vec![],
            place_by_class: false,
            tail_ticks: 0,
            scope: None,
            restore_workspace: Some(7),
        };
        let report = run_load(&host, &plan);
        assert_eq!(report.placed, 2);
        // Workspace 2 launches before workspace 4
        assert_eq!(execs(&host), vec!["kitty", "mpv"]);
        let sent = host.sent.borrow();
        assert!(sent.contains(&Op::MoveToWorkspace { window: a.address.clone(), workspace: crate::dispatch::WorkspaceTarget::Id(2) }));
        assert!(sent.contains(&Op::MoveWindow { window: b.address.clone(), x: 100, y: 200 }));
        assert!(sent.contains(&Op::ResizeWindow { window: b.address.clone(), w: 300, h: 400 }));
        // The tiled window gets its workspace only
        assert!(!sent.iter().any(|o| matches!(o, Op::MoveWindow { window, .. } if *window == a.address)));
        // Focus returns to the workspace that was active when load started
        assert_eq!(sent.last(), Some(&Op::FocusWorkspace(7)));
    }

    #[test]
    fn test_class_placement_handles_windows_that_arrive_late_and_reports_unmatched() {
        // Three saved brave windows; the app only recreates two of them
        let saved = vec![
            client("brave", "one", 1, false, (0, 0), (1, 1)),
            client("brave", "two", 2, false, (0, 0), (1, 1)),
            client("brave", "three", 3, false, (0, 0), (1, 1)),
        ];
        let live1 = client("brave", "one", 9, false, (1, 0), (1, 1));
        let live2 = client("brave", "two", 9, false, (2, 0), (1, 1));
        let host = FakeHost::new(vec![], vec![vec![(1, live1), (6, live2)]]);
        let plan = LoadPlan {
            entries: vec![entry("[workspace 1 silent] brave")],
            saved,
            place_by_class: true,
            tail_ticks: 10,
            scope: None,
            restore_workspace: None,
        };
        let report = run_load(&host, &plan);
        assert_eq!(report.placed, 2);
        assert_eq!(report.unmatched.len(), 1);
        assert_eq!(report.unmatched[0].title, "three");
    }

    #[test]
    fn test_load_ends_early_once_everything_is_matched() {
        let host = FakeHost::new(vec![], vec![vec![(1, client("kitty", "t", 9, false, (1, 0), (1, 1)))]]);
        let plan = LoadPlan {
            entries: vec![entry("[workspace 1 silent] kitty")],
            saved: vec![client("kitty", "t", 1, false, (0, 0), (1, 1))],
            place_by_class: true,
            tail_ticks: 600,
            scope: None,
            restore_workspace: None,
        };
        let report = run_load(&host, &plan);
        assert!(report.unmatched.is_empty());
        assert!(host.pauses.get() < 10, "waited {} ticks", host.pauses.get());
    }

    #[test]
    fn test_adjust_only_places_existing_windows_without_launching() {
        let live = client("kitty", "t", 5, false, (1, 0), (1, 1));
        let host = FakeHost::new(vec![live.clone()], vec![]);
        let plan = LoadPlan {
            entries: vec![],
            saved: vec![client("kitty", "t", 2, false, (0, 0), (1, 1))],
            place_by_class: true,
            tail_ticks: 5,
            scope: None,
            restore_workspace: None,
        };
        let report = run_load(&host, &plan);
        assert_eq!(report.placed, 1);
        assert!(execs(&host).is_empty());
    }

    #[test]
    fn test_launch_that_never_shows_a_window_does_not_stall_forever() {
        let host = FakeHost::new(vec![], vec![vec![]]);
        let plan = LoadPlan {
            entries: vec![entry("[workspace 1 silent] daemon")],
            saved: vec![],
            place_by_class: false,
            tail_ticks: 0,
            scope: None,
            restore_workspace: None,
        };
        let report = run_load(&host, &plan);
        assert_eq!(report.placed, 0);
        assert_eq!(host.pauses.get(), LAUNCH_WAIT_TICKS);
    }

    fn desktop(pids: &[(i32, &str)]) -> Vec<Client> {
        pids.iter()
            .enumerate()
            .map(|(i, (pid, class))| {
                let mut c = client(class, &format!("{class} {i}"), 1, false, (i as i16, 0), (1, 1));
                c.pid = *pid;
                c
            })
            .collect()
    }

    const FAST: ClearConfig = ClearConfig { close_ticks: 3, term_ticks: 3, kill_ticks: 3 };

    #[test]
    fn test_clear_on_empty_desktop_does_nothing() {
        let host = FakeHost::new(vec![], vec![]);
        let report = clear_session(&host, &FAST).unwrap();
        assert!(host.sent.borrow().is_empty() && host.signals.borrow().is_empty());
        assert!(report.closed.is_empty());
    }

    #[test]
    fn test_clear_closes_gracefully_without_any_signal() {
        let host = FakeHost::new(desktop(&[(10, "brave"), (20, "kitty"), (10, "brave")]), vec![]);
        let report = clear_session(&host, &FAST).unwrap();
        assert_eq!(report.closed.len(), 3);
        assert!(report.termed.is_empty() && report.killed.is_empty());
        assert!(host.signals.borrow().is_empty());
        let closes = host.sent.borrow().iter().filter(|o| matches!(o, Op::Close(_))).count();
        assert_eq!(closes, 3);
    }

    #[test]
    fn test_clear_escalates_only_for_what_stays() {
        // kitty closes; spotify ignores close but not SIGTERM; stubborn needs SIGKILL
        let host = FakeHost::new(desktop(&[(20, "kitty"), (30, "spotify"), (30, "spotify"), (40, "stubborn")]), vec![]);
        host.resist.borrow_mut().insert(30, 1);
        host.resist.borrow_mut().insert(40, 2);
        let report = clear_session(&host, &FAST).unwrap();
        assert_eq!(report.closed.len(), 1);
        // The two spotify windows share a pid: one SIGTERM, covering both stragglers
        // from the close stage; the pid that was already gone is never signalled
        assert_eq!(report.termed, vec![30, 40]);
        assert_eq!(report.killed, vec![40]);
        assert_eq!(
            *host.signals.borrow(),
            vec![(30, Signal::Term), (40, Signal::Term), (40, Signal::Kill)]
        );
        // Each wait was bounded by its stage
        assert!(host.pauses.get() <= FAST.close_ticks + FAST.term_ticks + FAST.kill_ticks + 3);
    }

    #[test]
    fn test_clear_escalates_in_order_close_then_term_then_kill() {
        let host = FakeHost::new(desktop(&[(50, "stubborn")]), vec![]);
        host.resist.borrow_mut().insert(50, 2);
        clear_session(&host, &FAST).unwrap();
        // The close request went out before any signal
        assert!(matches!(host.sent.borrow().first(), Some(Op::Close(_))));
        assert_eq!(*host.signals.borrow(), vec![(50, Signal::Term), (50, Signal::Kill)]);
    }

    #[test]
    fn test_clear_aborts_naming_survivors_of_sigkill() {
        let host = FakeHost::new(desktop(&[(60, "zombie"), (70, "kitty")]), vec![]);
        host.resist.borrow_mut().insert(60, 3);
        let err = clear_session(&host, &FAST).unwrap_err();
        assert!(err.contains("zombie") && err.contains("pid 60"), "{err}");
        assert!(!err.contains("kitty"), "{err}");
        assert_eq!(host.signals.borrow().last(), Some(&(60, Signal::Kill)));
    }

    #[test]
    fn test_clear_never_signals_init() {
        let host = FakeHost::new(desktop(&[(1, "odd"), (0, "odd")]), vec![]);
        host.resist.borrow_mut().insert(1, 3);
        host.resist.borrow_mut().insert(0, 3);
        assert!(clear_session(&host, &FAST).is_err());
        assert!(host.signals.borrow().is_empty());
    }
}
