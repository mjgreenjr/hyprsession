//! Loading a session: parse the launch lines, launch them in layout order, and place
//! every saved window as the applications bring their windows up.
//!
//! All interaction with the compositor goes through `Host`, so the sequencing can be
//! tested against a fake one.

use crate::dispatch::{Dispatcher, Op};
use crate::session::plan_adjustment;
use hyprland::data::{Client, Clients, FullscreenMode, Workspace};
use hyprland::shared::{Address, HyprData, HyprDataActive};
use std::collections::HashSet;

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
        Ok(ExecEntry { rules: parse_rules(rules)?, command: command.to_string() })
    } else if line.is_empty() {
        Err("empty line".to_string())
    } else {
        Ok(ExecEntry { rules: ExecRules::default(), command: line.to_string() })
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
    /// Wait one poll interval
    fn pause(&self);
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

    fn pause(&self) {
        std::thread::sleep(std::time::Duration::from_millis(1000 / TICKS_PER_SECOND));
    }
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
    }

    impl FakeHost {
        fn new(existing: Vec<Client>, scripts: Vec<Vec<(u64, Client)>>) -> Self {
            FakeHost {
                tick: Cell::new(0),
                windows: RefCell::new(existing.into_iter().map(|c| (0, c)).collect()),
                scripts: RefCell::new(scripts.into()),
                sent: RefCell::new(vec![]),
                pauses: Cell::new(0),
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
            self.sent.borrow_mut().push(op.clone());
            Ok(())
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
            restore_workspace: None,
        };
        let report = run_load(&host, &plan);
        assert_eq!(report.placed, 0);
        assert_eq!(host.pauses.get(), LAUNCH_WAIT_TICKS);
    }
}
