//use regex::Regex;
use std::fs;
use std::process::Command;
use hyprland::data::Client;
use std::collections::HashMap;
use crate::command_faker::xdg_id_key;

/// Check if a command exists in PATH using 'which'
pub fn command_exists_in_path(command: &str) -> bool {
    if command.is_empty() {
        return false;
    }
    
    Command::new("which")
        .arg(command.split_whitespace().next().unwrap())
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Extract the binary name from a command string (kept for the argv-less test helpers)
#[cfg(test)]
fn extract_binary_name(command: &str) -> String {
    let parts = command.split_whitespace();
    let binary = parts
        .clone()
        .next()
        .unwrap_or("")
        .split('/')
        .last()
        .unwrap_or("");

    format!("{} {}", binary, parts.skip(1).collect::<Vec<&str>>().join(" ")).trim().to_string()
}

/// Split raw `/proc/<pid>/cmdline` bytes into argv (NUL separated, NUL terminated)
pub fn parse_cmdline(raw: &[u8]) -> Vec<String> {
    let raw = raw.strip_suffix(&[0]).unwrap_or(raw);
    if raw.is_empty() {
        return vec![];
    }
    raw.split(|&b| b == 0)
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

/// The process argv, split on NUL before any name extraction
pub fn read_argv(pid: i32) -> Option<Vec<String>> {
    let raw = fs::read(format!("/proc/{}/cmdline", pid)).ok()?;
    let argv = parse_cmdline(&raw);
    if argv.is_empty() { None } else { Some(argv) }
}

/// The process working directory
pub fn read_cwd(pid: i32) -> Option<String> {
    fs::read_link(format!("/proc/{}/cwd", pid))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Quote one argument for a POSIX shell; arguments of safe characters stay bare
pub fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if safe {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// Join argv into a shell command line, quoting where needed
pub fn shell_join(argv: &[String]) -> String {
    argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

/// Build a launch command from argv: the binary name (path dropped) then quoted arguments
pub fn command_from_argv(argv: &[String]) -> Option<String> {
    let first = argv.first()?;
    let binary = first.rsplit('/').next().unwrap_or(first);
    let mut parts = vec![binary.to_string()];
    parts.extend(argv.iter().skip(1).cloned());
    Some(shell_join(&parts))
}

fn handle_proc_cmdline(client: &Client) -> Result<String, std::io::Error> {
    read_argv(client.pid)
        .and_then(|argv| command_from_argv(&argv))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "Cmdline not found"))
}

fn handle_proc_exe(client: &Client) -> Result<String, std::io::Error> {
    let exe_path = format!("/proc/{}/exe", client.pid);
    if let Ok(exe_target) = fs::read_link(&exe_path) {
        if let Some(exe_name) = exe_target.file_name() {
            return Ok(exe_name.to_string_lossy().to_string());
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "Exe not found"))
}

fn handle_initial_class(client: &Client) -> Result<String, std::io::Error> {
    Ok(client.initial_class.to_lowercase())
}

/// Look up a window class as a desktop file id (e.g. `dev.vencord.vesktop`)
fn resolve_class_by_app_id(class: &str, xdg_map: &HashMap<String, String>) -> Option<String> {
    if class.is_empty() {
        return None;
    }
    xdg_map.get(&xdg_id_key(class)).cloned()
}

/// The desktop-entry command for a window, resolved the way `fetch_command` resolves
/// it: desktop file id by class, then entry name by class, then entry name by title
pub fn resolve_desktop_command(client: &Client, xdg_map: &HashMap<String, String>) -> Option<String> {
    resolve_class_by_app_id(&client.initial_class, xdg_map)
        .or_else(|| resolve_class_by_app_id(&client.class, xdg_map))
        .or_else(|| xdg_map.get(&client.initial_class.to_lowercase()).cloned())
        .or_else(|| xdg_map.get(&client.initial_title.to_lowercase()).cloned())
}

fn handle_initial_title(client: &Client) -> Result<String, std::io::Error> {
    Ok(client.initial_title.to_lowercase())
}

/// Fetch command for a Hyprland client using multiple detection methods
pub fn fetch_command(client: &Client, xdg_map: &HashMap<String, String>) -> Result<String, std::io::Error> {
    type Handler = fn(&Client) -> Result<String, std::io::Error>;
    // (handler, whether the step also consults the desktop file id index)
    let handlers: [(Handler, bool); 4] = [
        (handle_proc_cmdline, false),
        (handle_proc_exe, false),
        (handle_initial_class, true),
        (handle_initial_title, false),
    ];

    for (handler, by_app_id) in handlers {
        if by_app_id {
            if let Some(command) = resolve_class_by_app_id(&client.initial_class, xdg_map) {
                return Ok(command);
            }
        }
        if let Ok(command) = handler(client) {
            if command_exists_in_path(&command) {
                return Ok(command);
            } else {
                // Fallback: check in xdg_map
                if let Some(xdg_command) = xdg_map.get(&command) {
                    return Ok(xdg_command.clone());
                }
            }
        }
    }

    // Fallback to cmdline even if not in PATH
    handle_proc_cmdline(client)
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_extract_binary_name() {
        assert_eq!(extract_binary_name("/usr/bin/firefox"), "firefox");
        assert_eq!(extract_binary_name("code"), "code");
        assert_eq!(extract_binary_name("/usr/bin/firefox --new-window"), "firefox --new-window");
        assert_eq!(extract_binary_name("/nix/store/.firefox-wrapped"), ".firefox-wrapped");
    }
    
    #[test]
    fn test_parse_cmdline_splits_on_nul() {
        assert_eq!(
            parse_cmdline(b"omawrite\0Job search activity.md\0"),
            vec!["omawrite".to_string(), "Job search activity.md".to_string()]
        );
        assert_eq!(parse_cmdline(b"a\0\0b\0"), vec!["a".to_string(), "".to_string(), "b".to_string()]);
        assert!(parse_cmdline(b"").is_empty());
    }

    #[test]
    fn test_argv_round_trips_through_shell_quoting() {
        let argv: Vec<String> = ["/usr/bin/omawrite", "Job search activity.md", "it's", "a\"b", "plain-1.x", ""]
            .iter().map(|s| s.to_string()).collect();
        let cmd = command_from_argv(&argv).unwrap();
        assert_eq!(cmd, r#"omawrite 'Job search activity.md' 'it'\''s' 'a"b' plain-1.x ''"#);

        // The quoted command line parses back to the original argv under a real shell
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\0' {}", cmd.splitn(2, ' ').nth(1).unwrap()))
            .output()
            .unwrap();
        assert_eq!(parse_cmdline(&out.stdout), argv[1..].to_vec());
    }

    #[test]
    fn test_shell_quote() {
        assert_eq!(shell_quote("simple"), "simple");
        assert_eq!(shell_quote("--flag=/a/b"), "--flag=/a/b");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn test_read_own_argv_and_cwd() {
        let pid = std::process::id() as i32;
        let argv = read_argv(pid).unwrap();
        assert!(!argv.is_empty());
        assert_eq!(read_cwd(pid).unwrap(), std::env::current_dir().unwrap().to_string_lossy());
    }

    #[test]
    fn test_class_resolves_to_own_desktop_entry() {
        // Vesktop-style: class is the desktop file id, title is a stale webapp's name
        let mut map = HashMap::new();
        map.insert(xdg_id_key("dev.vencord.vesktop"), "vesktop".to_string());
        map.insert("discord".to_string(), "chromium --app=https://discord.com".to_string());

        assert_eq!(resolve_class_by_app_id("dev.vencord.Vesktop", &map), Some("vesktop".to_string()));
        assert_eq!(resolve_class_by_app_id("unknown.app", &map), None);
        assert_eq!(resolve_class_by_app_id("", &map), None);
        // Name-keyed entries are not reachable through the class/id index
        assert_eq!(resolve_class_by_app_id("discord", &map), None);
    }

    /*
    #[test]
    fn test_handle_nix_wrapped() {
        // Test .name-wrapped format
        let cmd = "/nix/store/.firefox-wrapped --new-instance";
        let result = handle_nix_wrapped(cmd).unwrap();
        assert_eq!(result, "firefox --new-instance");
        
        // Test hash-name-wrapped format  
        let cmd_no_args = "/nix/store/xyz789-code-wrapped";
        let result_no_args = handle_nix_wrapped(cmd_no_args).unwrap();
        assert_eq!(result_no_args, "code");
        
        // Test full hash-name-wrapped format with args
        let cmd_full = "/nix/store/abc123-firefox-wrapped --new-window --private";
        let result_full = handle_nix_wrapped(cmd_full).unwrap();
        assert_eq!(result_full, "firefox --new-window --private");
        
        // Test simple .name-wrapped without args
        let cmd_simple = "/nix/store/randomhash/.chromium-wrapped";
        let result_simple = handle_nix_wrapped(cmd_simple).unwrap();
        assert_eq!(result_simple, "chromium");
    }
    
    #[test]
    fn test_handle_flatpak() {
        let cmd = "flatpak run org.mozilla.firefox --new-window";
        let result = handle_flatpak(cmd).unwrap();
        assert_eq!(result, "flatpak run org.mozilla.firefox --new-window");
        
        let cmd_with_flags = "flatpak run --app-id=org.gimp.GIMP --file=test.jpg";
        let result_with_flags = handle_flatpak(cmd_with_flags).unwrap();
        assert_eq!(result_with_flags, "flatpak run --app-id=org.gimp.GIMP --file=test.jpg");
    }
    
    #[test]
    fn test_handle_appimage() {
        let cmd = "/home/user/Applications/Firefox.AppImage --profile test";
        let result = handle_appimage(cmd).unwrap();
        assert_eq!(result, "Firefox.AppImage");
    }
    
    #[test]
    fn test_handle_snap() {
        let cmd = "/snap/firefox/current/usr/lib/firefox/firefox --new-window";
        let result = handle_snap(cmd).unwrap();
        assert_eq!(result, "firefox");
    }
    */
    #[test]
    #[ignore] // Weird bug when running as a nix flake
    fn test_command_exists_in_path() {
        // These commands should typically exist on most Linux systems
        assert!(command_exists_in_path("ls"));
        assert!(command_exists_in_path("cat"));
        assert!(command_exists_in_path("which"));
        
        // This command should not exist
        assert!(!command_exists_in_path("definitely_not_a_real_command_123456"));
        assert!(!command_exists_in_path(""));
    }
}