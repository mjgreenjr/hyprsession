use hyprland::data::{Client, Clients, FullscreenMode, Workspace};
use hyprland::shared::{HyprData, HyprDataActive};
use crate::command_detection::{fetch_command, read_argv, read_cwd};
use serde::{Deserialize, Serialize};
use crate::dispatch::{Dialect, Dispatcher, Op, WorkspaceTarget};
use crate::load::{desired_from_rules, launch_entries_from_saved, launch_order, placeholder_client, parse_exec_conf, run_load, clear_session, ClearConfig, LoadPlan, RealHost};
use std::collections::HashMap;
use std::path::PathBuf;

const EXEC_NAME: &str = "exec.conf";
const CLIENTS_PATH_NAME: &str = "clients.json";

pub trait Session {
    fn save(&self, name: &str) -> hyprland::Result<()>;
    fn load(&self, name: &str) -> hyprland::Result<()>;
    fn clear(&self) -> hyprland::Result<()>;
    fn list(&self) -> impl Iterator<Item=String>;
    fn delete(&self, name: &str);
}

macro_rules! iif {
    ($prop:expr, $val:expr) => {
        if $prop { $val } else { "" }
    };
    ($prop:expr, $val:expr, $alt:expr) => {
        if $prop { $val } else { $alt }
    };
}

macro_rules! to_base_dir {
    ($base_path:expr, $name:expr) => {
        if $name.is_empty() {
            PathBuf::from($base_path)
        } else {
            [$base_path, $name].iter().collect::<PathBuf>()
        }
    };
}

/// A client as stored in clients.json: the live client data plus what is needed to
/// relaunch it. The extra fields are optional because older snapshots lack them.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(try_from = "serde_json::Value", into = "serde_json::Value")]
pub struct SavedClient {
    pub client: Client,
    /// Process argv, split from the raw /proc/<pid>/cmdline
    pub argv: Option<Vec<String>>,
    /// Process working directory, the directory the window is launched from on load
    pub cwd: Option<String>,
    /// For terminal windows: the program running in the foreground of the terminal's tty
    pub foreground: Option<Foreground>,
}

/// The non-shell program that was in the foreground of a terminal when it was saved
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Foreground {
    /// Pid of the foreground process group leader at save time
    pub pid: i32,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    /// The program owns another saved client's window (by pid): relaunch nothing in the terminal
    #[serde(default)]
    pub linked: bool,
}

// The client fields and the extras share one JSON object. serde's flatten can't be
// used for that: it buffers values and the crate's monitor id is an i128.
impl From<SavedClient> for serde_json::Value {
    fn from(saved: SavedClient) -> Self {
        let mut value = serde_json::to_value(&saved.client).expect("Client serializes to JSON");
        let object = value.as_object_mut().expect("Client serializes to an object");
        if let Some(argv) = saved.argv {
            object.insert("argv".to_string(), serde_json::json!(argv));
        }
        if let Some(cwd) = saved.cwd {
            object.insert("cwd".to_string(), serde_json::json!(cwd));
        }
        if let Some(foreground) = saved.foreground {
            object.insert("foreground".to_string(), serde_json::json!(foreground));
        }
        value
    }
}

impl TryFrom<serde_json::Value> for SavedClient {
    type Error = serde_json::Error;

    fn try_from(mut value: serde_json::Value) -> Result<Self, Self::Error> {
        // Both extras are optional: snapshots from before they existed lack them
        let (argv, cwd, foreground) = match value.as_object_mut() {
            Some(object) => (
                object.remove("argv").and_then(|v| serde_json::from_value(v).ok()),
                object.remove("cwd").and_then(|v| serde_json::from_value(v).ok()),
                object.remove("foreground").and_then(|v| serde_json::from_value(v).ok()),
            ),
            None => (None, None, None),
        };
        Ok(SavedClient { client: serde_json::from_value(value)?, argv, cwd, foreground })
    }
}

impl SavedClient {
    pub fn capture(client: &Client) -> Self {
        SavedClient {
            client: client.clone(),
            argv: read_argv(client.pid),
            cwd: read_cwd(client.pid),
            foreground: None,
        }
    }
}

/// Order-independent description of a snapshot for comparing two saves: one sorted
/// entry per window of (class, workspace, floating, position, size, argv). Addresses,
/// pids and ordering are deliberately left out.
pub fn fingerprint(clients: &[SavedClient]) -> Vec<String> {
    let mut entries: Vec<String> = clients
        .iter()
        .map(|c| {
            let k = &c.client;
            format!(
                "{}|{}|{}|{:?}|{:?}|{:?}",
                k.class, k.workspace.id, k.floating, k.at, k.size, c.argv
            )
        })
        .collect();
    entries.sort();
    entries
}

/// Write a file by writing a sibling temp file then renaming it over the target,
/// so an interrupted save never leaves a truncated snapshot behind
fn write_atomic(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// Write both snapshot files for a session directory
fn write_snapshot(base_dir: &std::path::Path, exec_lines: &[String], saved: &[SavedClient]) -> std::io::Result<()> {
    let mut exec = String::new();
    for line in exec_lines {
        exec.push_str(line);
        exec.push('\n');
    }
    write_atomic(&base_dir.join(EXEC_NAME), exec.as_bytes())?;
    write_atomic(&base_dir.join(CLIENTS_PATH_NAME), &serde_json::to_vec(saved)?)
}

pub struct LocalSession {
    pub base_path: String,
    pub simulate: bool,
    pub load_time:  u64, 
    pub adjust_clients_only: bool,
    pub save_duplicate_pids: bool,
    pub dialect: Dialect,
}

/// Work out which dispatches bring a live window in line with its saved client
pub fn plan_adjustment(real_client: &Client, session_client: &Client) -> Vec<Op> {
    let window = real_client.address.clone();
    let mut ops = vec![];

    // Anonymous special workspace (id -99) has no documented Lua form: deferred
    let anonymous_special = session_client.workspace.id == -99;
    if anonymous_special {
        println!(
            "Warning: skipping workspace placement of '{}': anonymous special workspaces are not supported",
            real_client.title
        );
    } else {
        if real_client.workspace.id != session_client.workspace.id {
            let workspace = match session_client.workspace.name.strip_prefix("special:") {
                Some(name) => WorkspaceTarget::Special(name.to_string()),
                None => WorkspaceTarget::Id(session_client.workspace.id),
            };
            ops.push(Op::MoveToWorkspace { window: window.clone(), workspace });
        }
        if real_client.monitor != session_client.monitor && session_client.workspace.id > 0 {
            ops.push(Op::MoveWorkspaceToMonitor {
                workspace: session_client.workspace.id,
                monitor: session_client.monitor.unwrap_or(0),
            });
        }
    }
    if real_client.floating != session_client.floating {
        ops.push(Op::Float { window: window.clone(), enable: session_client.floating });
    }
    if real_client.pinned != session_client.pinned {
        ops.push(Op::Pin { window: window.clone() });
    }
    if real_client.fullscreen != session_client.fullscreen
        && session_client.fullscreen != FullscreenMode::None
    {
        ops.push(Op::Fullscreen {
            window: window.clone(),
            maximized: session_client.fullscreen == FullscreenMode::Maximized,
        });
    }
    // Tiled windows take their geometry from the layout: only floating ones are placed
    if session_client.floating && session_client.fullscreen == FullscreenMode::None {
        ops.push(Op::MoveWindow {
            window: window.clone(),
            x: session_client.at.0,
            y: session_client.at.1,
        });
        ops.push(Op::ResizeWindow {
            window,
            w: session_client.size.0,
            h: session_client.size.1,
        });
    }
    ops
}

impl LocalSession {
    /// Read and validate a snapshot, returning what load will do
    fn prepare_load(&self, base_dir: &std::path::Path) -> hyprland::Result<LoadPlan> {
        let fail = |msg: String| hyprland::error::HyprError::Other(msg);

        let saved: Vec<SavedClient> = match std::fs::read_to_string(base_dir.join(CLIENTS_PATH_NAME)) {
            Ok(data) => serde_json::from_str(&data)
                .map_err(|e| fail(format!("{} is not valid: {e}", CLIENTS_PATH_NAME)))?,
            Err(_) => vec![],
        };

        let mut entries = vec![];
        if !self.adjust_clients_only {
            let exec_path = base_dir.join(EXEC_NAME);
            let text = std::fs::read_to_string(&exec_path)
                .map_err(|e| fail(format!("cannot read {}: {e}", exec_path.display())))?;
            entries = parse_exec_conf(&text).map_err(fail)?;
            if entries.is_empty() {
                return Err(fail(format!("{} contains no launch lines", exec_path.display())));
            }
            for entry in entries.iter().filter(|e| !e.rules.unknown.is_empty()) {
                println!("Warning: ignoring unknown rules {:?} for '{}'", entry.rules.unknown, entry.command);
            }
        }

        // New-format snapshots (argv recorded) place every saved window by class.
        // Old ones only know their launch lines, so each launch's window is placed by
        // that line's rules.
        let new_format = saved.iter().any(|c| c.argv.is_some());
        let place_by_class = self.adjust_clients_only || new_format;

        // New-format snapshots launch exactly what was recorded (argv); exec.conf has
        // been validated above but its lines went through command heuristics and
        // remain the launch source only for legacy snapshots
        if new_format && !self.adjust_clients_only {
            entries = launch_entries_from_saved(&saved, &crate::command_faker::build_xdg_command_map());
        }

        Ok(LoadPlan {
            entries: launch_order(entries),
            saved: if place_by_class { saved.into_iter().map(|c| c.client).collect() } else { vec![] },
            place_by_class,
            tail_ticks: LoadPlan::tail_ticks_for(self.load_time),
            scope: None,
            restore_workspace: if self.simulate { None } else { Workspace::get_active().ok().map(|w| w.id) },
        })
    }
}

/// Print what a load would send. Windows to place do not exist yet, so their
/// dispatches are shown against a placeholder address.
fn simulate_load(dispatcher: &Dispatcher, plan: &LoadPlan) {
    for entry in &plan.entries {
        let _ = dispatcher.send(&Op::Exec(entry.command.clone()));
        if let Some(group) = entry.group.as_ref().filter(|g| g.saved.len() > 1) {
            println!(
                "Expecting {} windows from '{}' (would launch it again up to {} times if short)",
                group.saved.len(),
                group.label,
                group.saved.len() - 1
            );
        }
    }
    if plan.place_by_class {
        println!("After placement, windows matched by no saved client will be listed (not touched)");
        for wanted in &plan.saved {
            for op in plan_adjustment(&placeholder_client(), wanted) {
                println!("Would send once the window exists: dispatch {}", dispatcher.render(&op));
            }
        }
    } else {
        for entry in &plan.entries {
            let blank = placeholder_client();
            for op in plan_adjustment(&blank, &desired_from_rules(&blank, &entry.rules)) {
                println!("Would send once the window exists: dispatch {}", dispatcher.render(&op));
            }
        }
    }
}

/// Report launch groups that stayed short of their saved window count, with the saves left unmatched
fn print_shortfalls(shortfalls: &[crate::load::GroupShortfall], unmatched: &[Client]) {
    if shortfalls.is_empty() {
        return;
    }
    println!("Launch groups short of their saved window count: {}", shortfalls.len());
    for short in shortfalls {
        println!(
            "  '{}': {} of {} windows after {} extra launches",
            short.label, short.got, short.expected, short.reinvocations
        );
    }
    for client in unmatched {
        println!("  unmatched save: '{}' ({}) workspace={}", client.title, client.class, client.workspace.id);
    }
}

/// List live windows no saved client claimed. They are not touched.
fn print_surplus(surplus: &[Client]) {
    if surplus.is_empty() {
        return;
    }
    println!("Windows not matched to any saved client: {}", surplus.len());
    for window in surplus {
        println!(
            "  surplus {} class='{}' title='{}' workspace={}",
            window.address, window.class, window.title, window.workspace.id
        );
    }
}

impl Session for LocalSession {
    fn save(&self, name: &str) -> hyprland::Result<()> {
        println!("Saving session: {}", name);
        let base_dir: PathBuf = to_base_dir!(self.base_path.clone(), name.to_string());
        std::fs::create_dir_all(&base_dir).expect("Failed to create session directory");

        let props = [
            |info: &Client| format!("monitor {:?}", info.monitor.unwrap_or(0)),
            |info: &Client| iif!(info.workspace.id == -99,
                                format!("workspace special silent"),
                                format!("workspace {} silent", info.workspace.id)),
            |info: &Client| format!("{}", iif!(info.floating, "float")),
            |info: &Client| format!("move {} {}", info.at.0, info.at.1),
            |info: &Client| format!("size {} {}", info.size.0, info.size.1),
            |info: &Client| format!("{}", iif!(info.pinned, "pin")),
            |info: &Client| format!("fullscreenstate {}", info.fullscreen as i32),
        ];

        let client_info = Clients::get().expect("Unable to fetch clients");

        let mut pids: Vec<i32> = vec![];
        let mut saved_clients: Vec<SavedClient> = vec![];
        let mut exec_lines: Vec<String> = vec![];
        let xdg_map: HashMap<String, String> = crate::command_faker::build_xdg_command_map();

        for info in client_info.iter().rev() {
            // Every window is recorded in clients.json, whatever happens to its command
            saved_clients.push(SavedClient::capture(info));
            // exec.conf holds one launch line per distinct process
            if !self.save_duplicate_pids && pids.contains(&info.pid) {
                continue;
            }
            let cmd = fetch_command(info, &xdg_map);
            if !cmd.is_ok() {
                continue;
            }
            pids.push(info.pid);

            let exec_opts: Vec<String> = props
                .iter()
                .map(|opt| opt(info))
                .filter(|opt| !opt.is_empty())
                .collect();
            exec_lines.push(format!("[{}] {}", exec_opts.join(";"), cmd.unwrap()));
        }

        write_snapshot(&base_dir, &exec_lines, &saved_clients)
            .expect("Failed to write session files");
        println!("Session saved");
        Ok(())
    }

    fn load(&self, name: &str) -> hyprland::Result<()> {
        println!("Loading session: {}", name);
        let base_dir: PathBuf = to_base_dir!(self.base_path.clone(), name.to_string());
        let dispatcher = Dispatcher::new(self.dialect, self.simulate);

        // Everything is read and checked before anything is touched: a bad snapshot
        // must fail here, not after clear() has killed the desktop
        let plan = self.prepare_load(&base_dir)?;

        if self.simulate {
            simulate_load(&dispatcher, &plan);
        } else {
            if !self.adjust_clients_only {
                self.clear()?;
            }
            let host = RealHost { dispatcher };
            let report = run_load(&host, &plan);
            println!("Placed {} windows, {} saved windows not matched", report.placed, report.unmatched.len());
            print_shortfalls(&report.shortfalls, &report.unmatched);
            print_surplus(&report.surplus);
        }

        println!("Finished loading session");
        Ok(())
    }

    fn clear(&self) -> hyprland::Result<()> {
        if !self.simulate {
            let host = RealHost { dispatcher: Dispatcher::new(self.dialect, false) };
            clear_session(&host, &ClearConfig::default())
                .map_err(hyprland::error::HyprError::Other)?;
        }

        println!("Cleared existing session");
        Ok(())
    }

    fn list(&self) -> impl Iterator<Item=String> {
        let paths = std::fs::read_dir(self.base_path.clone()).unwrap();

        paths.filter_map(|path| {
            let entry = path.unwrap();
            if entry.path().is_dir() {
                Some(entry.file_name())
            } else {
                None
            }
        }).map(|os_str| os_str.to_string_lossy().into_owned())
    }

    fn delete(&self, name: &str) {
        let full_path = format!("{}/{}", self.base_path, name);
        if std::fs::remove_dir_all(&full_path).is_ok() {
            println!("Deleted session: {}", name);
        } else {
            println!("Failed to delete session: {}", name);
        }
    }
}
#[cfg(test)]
pub mod test_support {
    use hyprland::data::Client;

    /// Build a synthetic client; only the fields sessions care about are parameters
    pub fn client(class: &str, title: &str, ws: i32, floating: bool, at: (i16, i16), size: (i16, i16)) -> Client {
        serde_json::from_value(serde_json::json!({
            "address": format!("0x{:x}", (class.len() * 1000 + title.len() * 7 + at.0 as usize) as u32),
            "at": [at.0, at.1],
            "size": [size.0, size.1],
            "workspace": { "id": ws, "name": ws.to_string() },
            "floating": floating,
            "fullscreen": 0,
            "fullscreenClient": 0,
            "monitor": 0,
            "initialClass": class,
            "class": class,
            "initialTitle": title,
            "title": title,
            "pid": 100,
            "xwayland": false,
            "pinned": false,
            "grouped": [],
            "mapped": true,
            "swallowing": null,
            "focusHistoryID": 0
        })).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_support::client;

    #[test]
    fn test_saved_client_round_trip_with_argv_and_cwd() {
        let saved = SavedClient {
            client: client("brave", "tab", 2, false, (0, 0), (10, 10)),
            argv: Some(vec!["brave".into(), "--user-data-dir=/x y".into()]),
            cwd: Some("/home/synthetic".into()),
            foreground: None,
        };
        let json = serde_json::to_string(&vec![saved.clone()]).unwrap();
        let back: Vec<SavedClient> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[0].argv, saved.argv);
        assert_eq!(back[0].cwd, saved.cwd);
        assert_eq!(back[0].client, saved.client);
        // Still readable as a plain client list by older consumers
        let plain: Vec<Client> = serde_json::from_str(&json).unwrap();
        assert_eq!(plain[0], saved.client);
    }

    fn sim_session(dir: &std::path::Path) -> LocalSession {
        LocalSession {
            base_path: dir.to_str().unwrap().to_string(),
            simulate: true,
            load_time: 1,
            adjust_clients_only: false,
            save_duplicate_pids: false,
            dialect: Dialect::Lua,
        }
    }

    fn write_clients(dir: &std::path::Path, clients: &[SavedClient]) {
        std::fs::write(dir.join(CLIENTS_PATH_NAME), serde_json::to_vec(clients).unwrap()).unwrap();
    }

    fn with_argv(class: &str, argv: &[&str], ws: i32, at: (i16, i16)) -> SavedClient {
        SavedClient {
            client: client(class, "t", ws, false, at, (10, 10)),
            argv: Some(argv.iter().map(|s| s.to_string()).collect()),
            cwd: None,
            foreground: None,
        }
    }

    #[test]
    fn test_new_format_launches_every_distinct_argv_despite_stale_exec_conf() {
        let dir = tempfile::tempdir().unwrap();
        // exec.conf is stale and degraded: one bare 'brave', nothing else
        std::fs::write(dir.path().join(EXEC_NAME), "[workspace 1 silent] brave\n").unwrap();
        write_clients(dir.path(), &[
            with_argv("brave", &["/opt/brave/brave", "--app=https://example.test"], 5, (0, 0)),
            with_argv("brave", &["/opt/brave/brave", "--user-data-dir=/home/x/profile two"], 1, (0, 0)),
            with_argv("brave", &["/opt/brave/brave", "--user-data-dir=/home/x/profile two"], 1, (50, 0)),
            with_argv("kitty", &["kitty"], 2, (0, 0)),
        ]);
        let plan = sim_session(dir.path()).prepare_load(dir.path()).unwrap();
        assert!(plan.place_by_class);
        let commands: Vec<&str> = plan.entries.iter().map(|e| e.command.as_str()).collect();
        // Launch order: workspace ascending
        assert_eq!(commands, vec![
            "/opt/brave/brave '--user-data-dir=/home/x/profile two'",
            "kitty",
            "/opt/brave/brave --app=https://example.test",
        ]);
        assert!(!commands.contains(&"brave"));
    }

    #[test]
    fn test_new_format_still_requires_a_valid_exec_conf() {
        let dir = tempfile::tempdir().unwrap();
        write_clients(dir.path(), &[with_argv("kitty", &["kitty"], 1, (0, 0))]);
        // Missing
        assert!(sim_session(dir.path()).prepare_load(dir.path()).is_err());
        // Unparseable
        std::fs::write(dir.path().join(EXEC_NAME), "[move a b] x\n").unwrap();
        assert!(sim_session(dir.path()).prepare_load(dir.path()).is_err());
        // Empty
        std::fs::write(dir.path().join(EXEC_NAME), "\n").unwrap();
        assert!(sim_session(dir.path()).prepare_load(dir.path()).is_err());
        // Valid
        std::fs::write(dir.path().join(EXEC_NAME), "[workspace 1 silent] kitty\n").unwrap();
        assert!(sim_session(dir.path()).prepare_load(dir.path()).is_ok());
    }

    #[test]
    fn test_legacy_snapshot_follows_the_exec_conf_path() {
        let dir = tempfile::tempdir().unwrap();
        let exec = "[workspace 2 silent;float;move 5 6;size 7 8] kitty --title 'a b'\n[workspace 1 silent;move 0 0] brave\n";
        std::fs::write(dir.path().join(EXEC_NAME), exec).unwrap();
        // Old-format clients.json: windows but no argv anywhere
        write_clients(dir.path(), &[
            SavedClient { client: client("kitty", "t", 2, true, (5, 6), (7, 8)), argv: None, cwd: None, foreground: None },
        ]);
        let plan = sim_session(dir.path()).prepare_load(dir.path()).unwrap();
        assert!(!plan.place_by_class);
        assert!(plan.saved.is_empty());
        assert_eq!(plan.entries, launch_order(parse_exec_conf(exec).unwrap()));
        let commands: Vec<&str> = plan.entries.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(commands, vec!["brave", "kitty --title 'a b'"]);
    }

    #[test]
    fn test_fingerprint_ignores_address_pid_and_order_but_not_geometry() {
        let saved = |class: &str, at: (i16, i16), pid: i32| {
            let mut client = client(class, "t", 1, false, at, (10, 10));
            client.pid = pid;
            SavedClient { client, argv: Some(vec![class.to_string()]), cwd: None, foreground: None }
        };
        let before = vec![saved("brave", (0, 0), 10), saved("brave", (5, 0), 10), saved("kitty", (9, 9), 20)];
        // After a reload: new pids, new addresses, different order
        let mut after = vec![saved("kitty", (9, 9), 77), saved("brave", (5, 0), 55), saved("brave", (0, 0), 55)];
        for (i, c) in after.iter_mut().enumerate() {
            c.client.address = hyprland::shared::Address::new(format!("0xf{i}"));
        }
        assert_eq!(fingerprint(&before), fingerprint(&after));

        after[1].client.at = (6, 0);
        assert_ne!(fingerprint(&before), fingerprint(&after));
    }

    #[test]
    fn test_old_snapshot_without_new_fields_still_reads() {
        let old = serde_json::to_string(&vec![client("kitty", "t", 1, false, (1, 2), (3, 4))]).unwrap();
        let back: Vec<SavedClient> = serde_json::from_str(&old).unwrap();
        assert_eq!(back.len(), 1);
        assert!(back[0].argv.is_none() && back[0].cwd.is_none());
        // And absent fields are not written back out
        assert!(!serde_json::to_string(&back).unwrap().contains("argv"));
    }

    #[test]
    fn test_write_snapshot_is_atomic_and_complete() {
        let dir = tempfile::tempdir().unwrap();
        let clients: Vec<SavedClient> = (0..3)
            .map(|i| SavedClient {
                client: client("brave", &format!("w{i}"), 1, false, (i, 0), (10, 10)),
                argv: Some(vec!["brave".into()]),
                cwd: None,
                foreground: None,
            })
            .collect();
        // Three windows of one process: one launch line, three saved windows
        write_snapshot(dir.path(), &["[workspace 1 silent] brave".to_string()], &clients).unwrap();
        let back: Vec<SavedClient> =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(CLIENTS_PATH_NAME)).unwrap()).unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(EXEC_NAME)).unwrap(),
            "[workspace 1 silent] brave\n"
        );
        assert!(!dir.path().join("exec.conf.tmp").exists());
    }

    #[test]
    fn test_plan_adjustment_tiled_gets_workspace_only() {
        let real = client("kitty", "term", 1, false, (0, 0), (500, 500));
        let saved = client("kitty", "term", 4, false, (10, 20), (300, 300));
        let ops = plan_adjustment(&real, &saved);
        assert_eq!(ops, vec![Op::MoveToWorkspace {
            window: real.address.clone(),
            workspace: WorkspaceTarget::Id(4),
        }]);
    }

    #[test]
    fn test_plan_adjustment_floating_gets_float_move_resize() {
        let real = client("mpv", "video", 1, false, (0, 0), (500, 500));
        let saved = client("mpv", "video", 2, true, (10, 20), (300, 400));
        let ops = plan_adjustment(&real, &saved);
        let a = real.address.clone();
        assert_eq!(ops, vec![
            Op::MoveToWorkspace { window: a.clone(), workspace: WorkspaceTarget::Id(2) },
            Op::Float { window: a.clone(), enable: true },
            Op::MoveWindow { window: a.clone(), x: 10, y: 20 },
            Op::ResizeWindow { window: a, w: 300, h: 400 },
        ]);
    }

    #[test]
    fn test_plan_adjustment_nothing_to_do_when_tiled_and_in_place() {
        let real = client("kitty", "term", 3, false, (0, 0), (500, 500));
        let saved = client("kitty", "term", 3, false, (5, 5), (100, 100));
        assert!(plan_adjustment(&real, &saved).is_empty());
    }

    #[test]
    fn test_plan_adjustment_named_and_anonymous_special_workspaces() {
        let real = client("a", "t", 1, false, (0, 0), (1, 1));
        let mut saved = client("a", "t", -98, false, (0, 0), (1, 1));
        saved.workspace.name = "special:scratch".to_string();
        assert_eq!(plan_adjustment(&real, &saved), vec![Op::MoveToWorkspace {
            window: real.address.clone(),
            workspace: WorkspaceTarget::Special("scratch".to_string()),
        }]);

        let mut anon = client("a", "t", -99, false, (0, 0), (1, 1));
        anon.workspace.name = "special".to_string();
        assert!(plan_adjustment(&real, &anon).is_empty());
    }
}
