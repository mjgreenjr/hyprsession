//! Dispatch layer: renders session operations in the dialect understood by the
//! running Hyprland (Lua config provider or classic) and sends them.
//!
//! Data queries and the event listener still go through the hyprland crate; only
//! dispatches live here.

use clap::ValueEnum;
use hyprland::dispatch::{
    Dispatch, DispatchType, FullscreenType, Position, WindowIdentifier, WorkspaceIdentifier,
    WorkspaceIdentifierWithSpecial, MonitorIdentifier,
};
use hyprland::shared::Address;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Dialect {
    Lua,
    Classic,
}

impl std::fmt::Display for Dialect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Dialect::Lua => write!(f, "lua"),
            Dialect::Classic => write!(f, "classic"),
        }
    }
}

/// Value of the `--dialect` option
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum DialectChoice {
    Auto,
    Lua,
    Classic,
}

/// Where a resolved dialect came from; printed so the choice is visible
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DialectSource {
    Flag,
    Probe,
    Guess,
}

impl std::fmt::Display for DialectSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DialectSource::Flag => write!(f, "flag"),
            DialectSource::Probe => write!(f, "probe"),
            DialectSource::Guess => write!(f, "guess, simulate"),
        }
    }
}

/// Target workspace of a window move
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceTarget {
    Id(i32),
    /// Named special workspace, without the `special:` prefix
    Special(String),
}

/// A single dispatch the program can perform
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Exec(String),
    /// Move a window to a workspace without following it
    MoveToWorkspace { window: Address, workspace: WorkspaceTarget },
    /// Set floating on or off. Classic can only toggle, so callers must only issue
    /// this when the live state differs from the wanted one.
    Float { window: Address, enable: bool },
    MoveWindow { window: Address, x: i16, y: i16 },
    ResizeWindow { window: Address, w: i16, h: i16 },
    /// Toggles pinning
    Pin { window: Address },
    /// `maximized` selects maximize rather than real fullscreen
    Fullscreen { window: Address, maximized: bool },
    FocusWorkspace(i32),
    FocusWindow(Address),
    MoveWorkspaceToMonitor { workspace: i32, monitor: i128 },
    Close(Address),
}

/// Quote a string as a single-quoted Lua string literal
pub fn lua_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

fn lua_window(addr: &Address) -> String {
    lua_str(&format!("address:{addr}"))
}

/// Render an op as the text following `dispatch ` for the Lua provider
pub fn render_lua(op: &Op) -> String {
    match op {
        Op::Exec(cmd) => format!("hl.dsp.exec_cmd({})", lua_str(cmd)),
        Op::MoveToWorkspace { window, workspace } => {
            let ws = match workspace {
                WorkspaceTarget::Id(id) => id.to_string(),
                WorkspaceTarget::Special(name) => lua_str(&format!("special:{name}")),
            };
            format!(
                "hl.dsp.window.move({{ workspace = {ws}, window = {}, follow = false }})",
                lua_window(window)
            )
        }
        Op::Float { window, enable } => format!(
            "hl.dsp.window.float({{ action = '{}', window = {} }})",
            if *enable { "enable" } else { "disable" },
            lua_window(window)
        ),
        Op::MoveWindow { window, x, y } => format!(
            "hl.dsp.window.move({{ window = {}, x = {x}, y = {y} }})",
            lua_window(window)
        ),
        Op::ResizeWindow { window, w, h } => format!(
            "hl.dsp.window.resize({{ window = {}, x = {w}, y = {h} }})",
            lua_window(window)
        ),
        Op::Pin { window } => format!("hl.dsp.window.pin({{ window = {} }})", lua_window(window)),
        Op::Fullscreen { window, maximized } => format!(
            "hl.dsp.window.fullscreen({{ mode = '{}', action = 'enable', window = {} }})",
            if *maximized { "maximized" } else { "fullscreen" },
            lua_window(window)
        ),
        Op::FocusWorkspace(id) => format!("hl.dsp.focus({{ workspace = {id} }})"),
        Op::FocusWindow(window) => format!("hl.dsp.focus({{ window = {} }})", lua_window(window)),
        Op::MoveWorkspaceToMonitor { workspace, monitor } => format!(
            "hl.dsp.workspace.move({{ workspace = {workspace}, monitor = {monitor} }})"
        ),
        Op::Close(window) => format!("hl.dsp.window.close({{ window = {} }})", lua_window(window)),
    }
}

fn classic_call(op: &Op) -> hyprland::Result<()> {
    let ident = |a: &Address| WindowIdentifier::Address(a.clone());
    match op {
        Op::Exec(cmd) => Dispatch::call(DispatchType::Exec(cmd)),
        Op::MoveToWorkspace { window, workspace } => match workspace {
            WorkspaceTarget::Id(id) => Dispatch::call(DispatchType::MoveToWorkspaceSilent(
                WorkspaceIdentifierWithSpecial::Id(*id),
                Some(ident(window)),
            )),
            WorkspaceTarget::Special(name) => Dispatch::call(DispatchType::MoveToWorkspaceSilent(
                WorkspaceIdentifierWithSpecial::Special(Some(name)),
                Some(ident(window)),
            )),
        },
        Op::Float { window, .. } => Dispatch::call(DispatchType::ToggleFloating(Some(ident(window)))),
        Op::MoveWindow { window, x, y } => {
            Dispatch::call(DispatchType::MoveWindowPixel(Position::Exact(*x, *y), ident(window)))
        }
        Op::ResizeWindow { window, w, h } => {
            Dispatch::call(DispatchType::ResizeWindowPixel(Position::Exact(*w, *h), ident(window)))
        }
        Op::Pin { window } => Dispatch::call(DispatchType::TogglePinWindow(ident(window))),
        Op::Fullscreen { window, maximized } => {
            Dispatch::call(DispatchType::FocusWindow(ident(window)))?;
            Dispatch::call(DispatchType::ToggleFullscreen(if *maximized {
                FullscreenType::Maximize
            } else {
                FullscreenType::Real
            }))
        }
        Op::FocusWorkspace(id) => {
            Dispatch::call(DispatchType::Workspace(WorkspaceIdentifierWithSpecial::Id(*id)))
        }
        Op::FocusWindow(window) => Dispatch::call(DispatchType::FocusWindow(ident(window))),
        Op::MoveWorkspaceToMonitor { workspace, monitor } => {
            Dispatch::call(DispatchType::MoveWorkspaceToMonitor(
                WorkspaceIdentifier::Id(*workspace),
                MonitorIdentifier::Id(*monitor),
            ))
        }
        Op::Close(window) => {
            Dispatch::call(DispatchType::CloseWindow(ident(window)))
        }
    }
}

/// Path of the Hyprland command socket for the current instance
pub fn command_socket_path() -> Option<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let base = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(dir) => PathBuf::from(dir).join("hypr"),
        Err(_) => PathBuf::from("/tmp/hypr"),
    };
    Some(base.join(sig).join(".socket.sock"))
}

/// Send `/dispatch <text>` over the command socket and return the raw reply
pub fn send_raw_dispatch(socket: &Path, text: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(format!("/dispatch {text}").as_bytes())?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply)?;
    Ok(String::from_utf8_lossy(&reply).into_owned())
}

/// Classify a dispatch reply: exactly `ok` is accepted, anything else is not
pub fn reply_is_ok(reply: &str) -> bool {
    reply.trim() == "ok"
}

const CLASSIC_PROBE: &str = "submap reset";
const LUA_PROBE: &str = "hl.dsp.exec_cmd('true')";

fn probe(socket: &Path, text: &str) -> bool {
    send_raw_dispatch(socket, text).map(|r| reply_is_ok(&r)).unwrap_or(false)
}

/// Decide the dialect from the compositor's replies: classic no-op first, then the
/// Lua no-op to confirm. `send` returns whether the probe text was accepted.
pub fn detect_dialect(mut accepted: impl FnMut(&str) -> bool) -> Option<Dialect> {
    if accepted(CLASSIC_PROBE) {
        Some(Dialect::Classic)
    } else if accepted(LUA_PROBE) {
        Some(Dialect::Lua)
    } else {
        None
    }
}

/// Read-only guess used where nothing may be sent to the compositor
fn guess_dialect() -> Dialect {
    let home = std::env::var("HOME").unwrap_or_default();
    let config_home = std::env::var("XDG_CONFIG_HOME").unwrap_or(format!("{home}/.config"));
    if Path::new(&config_home).join("hypr/hyprland.lua").exists() {
        Dialect::Lua
    } else {
        Dialect::Classic
    }
}

/// Resolve the dialect to use. Errors if probing finds neither dialect accepted.
pub fn resolve_dialect(choice: DialectChoice, simulate: bool) -> Result<(Dialect, DialectSource), String> {
    match choice {
        DialectChoice::Lua => Ok((Dialect::Lua, DialectSource::Flag)),
        DialectChoice::Classic => Ok((Dialect::Classic, DialectSource::Flag)),
        DialectChoice::Auto if simulate => Ok((guess_dialect(), DialectSource::Guess)),
        DialectChoice::Auto => {
            let socket = command_socket_path()
                .ok_or("HYPRLAND_INSTANCE_SIGNATURE is not set; is Hyprland running?")?;
            detect_dialect(|text| probe(&socket, text))
                .map(|d| (d, DialectSource::Probe))
                .ok_or_else(|| {
                    "Could not determine the dispatch dialect: the compositor rejected both the classic and Lua probes. Use --dialect to force one.".to_string()
                })
        }
    }
}

/// Sends ops in the chosen dialect, or only prints them when simulating
#[derive(Clone, Debug)]
pub struct Dispatcher {
    pub dialect: Dialect,
    pub simulate: bool,
    /// Overrides the discovered socket (used by tests)
    pub socket: Option<PathBuf>,
}

impl Dispatcher {
    pub fn new(dialect: Dialect, simulate: bool) -> Self {
        Dispatcher { dialect, simulate, socket: None }
    }

    /// Text sent after `dispatch ` (Lua dialect) or a description of the call (classic)
    pub fn render(&self, op: &Op) -> String {
        match self.dialect {
            Dialect::Lua => render_lua(op),
            Dialect::Classic => format!("{op:?}"),
        }
    }

    pub fn send(&self, op: &Op) -> hyprland::Result<()> {
        println!("Sending: dispatch {}", self.render(op));
        if self.simulate {
            return Ok(());
        }
        match self.dialect {
            Dialect::Classic => classic_call(op),
            Dialect::Lua => {
                let socket = self
                    .socket
                    .clone()
                    .or_else(command_socket_path)
                    .ok_or_else(|| hyprland::error::HyprError::Other(
                        "HYPRLAND_INSTANCE_SIGNATURE is not set".to_string(),
                    ))?;
                let reply = send_raw_dispatch(&socket, &render_lua(op))?;
                if reply_is_ok(&reply) {
                    Ok(())
                } else {
                    Err(hyprland::error::HyprError::NotOkDispatch(reply))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> Address {
        Address::new("0xabc123")
    }

    #[test]
    fn test_lua_string_forms_match_verified_shapes() {
        assert_eq!(render_lua(&Op::Exec("kitty".into())), "hl.dsp.exec_cmd('kitty')");
        assert_eq!(
            render_lua(&Op::MoveToWorkspace { window: addr(), workspace: WorkspaceTarget::Id(5) }),
            "hl.dsp.window.move({ workspace = 5, window = 'address:0xabc123', follow = false })"
        );
        assert_eq!(
            render_lua(&Op::MoveToWorkspace { window: addr(), workspace: WorkspaceTarget::Special("magic".into()) }),
            "hl.dsp.window.move({ workspace = 'special:magic', window = 'address:0xabc123', follow = false })"
        );
        assert_eq!(
            render_lua(&Op::Float { window: addr(), enable: true }),
            "hl.dsp.window.float({ action = 'enable', window = 'address:0xabc123' })"
        );
        assert_eq!(
            render_lua(&Op::MoveWindow { window: addr(), x: 10, y: -20 }),
            "hl.dsp.window.move({ window = 'address:0xabc123', x = 10, y = -20 })"
        );
        assert_eq!(
            render_lua(&Op::ResizeWindow { window: addr(), w: 800, h: 600 }),
            "hl.dsp.window.resize({ window = 'address:0xabc123', x = 800, y = 600 })"
        );
        assert_eq!(render_lua(&Op::FocusWorkspace(3)), "hl.dsp.focus({ workspace = 3 })");
        assert_eq!(
            render_lua(&Op::Close(addr())),
            "hl.dsp.window.close({ window = 'address:0xabc123' })"
        );
    }

    #[test]
    fn test_lua_forms_from_documentation() {
        // Taken from the dispatcher reference, not yet confirmed on a live compositor
        assert_eq!(
            render_lua(&Op::Pin { window: addr() }),
            "hl.dsp.window.pin({ window = 'address:0xabc123' })"
        );
        assert_eq!(
            render_lua(&Op::Fullscreen { window: addr(), maximized: false }),
            "hl.dsp.window.fullscreen({ mode = 'fullscreen', action = 'enable', window = 'address:0xabc123' })"
        );
        assert_eq!(
            render_lua(&Op::MoveWorkspaceToMonitor { workspace: 2, monitor: 1 }),
            "hl.dsp.workspace.move({ workspace = 2, monitor = 1 })"
        );
    }

    #[test]
    fn test_lua_quoting_and_escaping() {
        assert_eq!(lua_str("plain"), "'plain'");
        assert_eq!(lua_str("it's"), "'it\\'s'");
        assert_eq!(lua_str(r"back\slash"), r"'back\\slash'");
        assert_eq!(lua_str("a\nb"), "'a\\nb'");
        assert_eq!(
            render_lua(&Op::Exec(r#"omawrite 'Job search.md' "x" \n"#.into())),
            r#"hl.dsp.exec_cmd('omawrite \'Job search.md\' "x" \\n')"#
        );
        // A trailing backslash must not escape the closing quote
        assert_eq!(lua_str("end\\"), "'end\\\\'");
    }

    #[test]
    fn test_reply_classification() {
        assert!(reply_is_ok("ok"));
        assert!(reply_is_ok("ok\n"));
        assert!(!reply_is_ok("error: [string \"return hl.dispatch(submap reset)\"]:1: ')' expected near 'reset'"));
        assert!(!reply_is_ok(""));
    }

    #[test]
    fn test_detect_dialect() {
        // Classic build accepts the classic probe
        assert_eq!(detect_dialect(|t| t == CLASSIC_PROBE), Some(Dialect::Classic));
        // Lua build rejects classic text but accepts the Lua no-op
        assert_eq!(detect_dialect(|t| t == LUA_PROBE), Some(Dialect::Lua));
        // Neither accepted: no guess
        assert_eq!(detect_dialect(|_| false), None);
    }

    #[test]
    fn test_simulate_sends_nothing() {
        let mut d = Dispatcher::new(Dialect::Lua, true);
        d.socket = Some(PathBuf::from("/nonexistent/socket"));
        assert!(d.send(&Op::Exec("true".into())).is_ok());
    }

    #[test]
    fn test_send_over_socket() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = std::thread::spawn(move || {
            let mut seen = vec![];
            for (reply, _) in [("ok", 0), ("error: nope", 1)] {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).unwrap();
                seen.push(String::from_utf8_lossy(&buf[..n]).into_owned());
                s.write_all(reply.as_bytes()).unwrap();
            }
            seen
        });

        let mut d = Dispatcher::new(Dialect::Lua, false);
        d.socket = Some(path);
        assert!(d.send(&Op::Exec("it's".into())).is_ok());
        assert!(d.send(&Op::FocusWorkspace(1)).is_err());
        assert_eq!(
            handle.join().unwrap(),
            vec![
                "/dispatch hl.dsp.exec_cmd('it\\'s')".to_string(),
                "/dispatch hl.dsp.focus({ workspace = 1 })".to_string(),
            ]
        );
    }
}
