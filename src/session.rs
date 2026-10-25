use hyprland::data::{Client, Clients, FullscreenMode};
use hyprland::event_listener::EventListener;
use hyprland::prelude::*;
use hyprland::shared::Address;
use std::fs::File;
use std::io::read_to_string;
use crate::command_detection::{fetch_command, read_argv, read_cwd};
use serde::{Deserialize, Serialize};
use crate::dispatch::{Dialect, Dispatcher, Op, WorkspaceTarget};
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
    /// Process working directory (recorded only; not used when loading)
    pub cwd: Option<String>,
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
        value
    }
}

impl TryFrom<serde_json::Value> for SavedClient {
    type Error = serde_json::Error;

    fn try_from(mut value: serde_json::Value) -> Result<Self, Self::Error> {
        // Both extras are optional: snapshots from before they existed lack them
        let (argv, cwd) = match value.as_object_mut() {
            Some(object) => (
                object.remove("argv").and_then(|v| serde_json::from_value(v).ok()),
                object.remove("cwd").and_then(|v| serde_json::from_value(v).ok()),
            ),
            None => (None, None),
        };
        Ok(SavedClient { client: serde_json::from_value(value)?, argv, cwd })
    }
}

impl SavedClient {
    pub fn capture(client: &Client) -> Self {
        SavedClient {
            client: client.clone(),
            argv: read_argv(client.pid),
            cwd: read_cwd(client.pid),
        }
    }
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

fn adjust_client(dispatcher: &Dispatcher, real_client: &Client, session_client: &Client) {
    println!("found '{}' on workspace: {}", real_client.title, real_client.workspace.id);
    for op in plan_adjustment(real_client, session_client) {
        if dispatcher.send(&op).is_err() {
            println!("Warning: Failed to apply {:?}: {}", op, real_client.title);
        }
    }
}

fn process_window_event(dispatcher: &Dispatcher, address: Address, clients_data: &'static str, start_time: std::time::Instant, load_time: u64) {
    let clients: Vec<Client> = serde_json::from_str(clients_data)
        .expect("Failed to parse clients data");

    if start_time.elapsed().as_secs() > load_time { 
        println!("Load time exceeded, skipping client adjustments");
        return;
    }

    let real_clients = Clients::get()
        .expect("Unable to fetch clients");
    if let Some(real_client) = real_clients.iter().find(|c| c.address == address) {
        for session_client in clients.iter() {
            if session_client.title == real_client.title {
                println!("Adjusting client: {:?}", real_client.title);
                adjust_client(dispatcher, real_client, session_client);
                break;
            } else {
                println!("Client '{:?}' not found - skipping", address);
            }
        }
    }
}

fn load_programs(dispatcher: &Dispatcher, base_path: &PathBuf) -> hyprland::Result<()> {
    let session_file = File::open(base_path.join(EXEC_NAME));

    if session_file.is_ok() {
        for line in read_to_string(session_file.unwrap()).unwrap().lines() {
            dispatcher.send(&Op::Exec(line.to_string()))?;
        }
    }

    Ok(())
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
        let start_time = std::time::Instant::now();

        if !self.adjust_clients_only {
            self.clear()?;
            load_programs(&Dispatcher::new(self.dialect, self.simulate), &base_dir.clone())?;
        }

        // Clone the values we need inside the thread before moving into the closure
        let load_time = self.load_time;
        let dispatcher = Dispatcher::new(self.dialect, self.simulate);
        
        std::thread::spawn(move || {
            let clients_file_path = base_dir.join(CLIENTS_PATH_NAME);
            let clients_data = std::fs::read_to_string(&clients_file_path)
                .unwrap_or_else(|_| "[]".to_string());
            let clients_data: &'static str = Box::leak(clients_data.into_boxed_str());

            let mut event_listener = EventListener::new();
            let title_dispatcher = dispatcher.clone();
            event_listener.add_window_title_changed_handler({move |event| {
                println!("Window title changed: {}", event.address);
                process_window_event(&title_dispatcher, event.address, clients_data, start_time, load_time);
            }});
            event_listener.add_window_opened_handler({move |event| {
                println!("Window opened: {}", event.window_address);
                process_window_event(&dispatcher, event.window_address, clients_data, start_time, load_time);
            }});
            let _ = event_listener.start_listener();
        });

        std::thread::sleep(std::time::Duration::from_secs(self.load_time + 1));
        println!("Finished loading session");
        Ok(())
    }

    fn clear(&self) -> hyprland::Result<()> {
        if !self.simulate {
            let pids: Vec<i32> = Clients::get()?
                .iter()
                .map(|c| c.pid)
                .collect();
            for pid in pids {
                let _ = std::process::Command::new("kill")
                    .arg(pid.to_string())
                    .output()
                    .unwrap();
            }

            loop {
                if Clients::get()?.iter().collect::<Vec<&Client>>().is_empty() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
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
