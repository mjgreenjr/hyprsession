use std::fs;
use std::path::PathBuf;
use std::os::unix::fs::PermissionsExt;
use std::collections::HashMap;

pub fn fake_command(name: &str, command: &str) -> Result<(), Box<dyn std::error::Error>> {
    let home_dir = std::env::var("HOME")?;
    let bin_path = PathBuf::from(home_dir).join(".local/bin");
    
    // Create directory if it doesn't exist
    fs::create_dir_all(&bin_path)?;
        
    let file_path = bin_path.join(name);
    if !command.is_empty() {
        // Write the command to the file with shebang
        let content = format!("#!/bin/sh\n{}", command);
        fs::write(&file_path, content)?;

        // Make the file executable
        let mut perms = fs::metadata(&file_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&file_path, perms)?;
    } else {
        // Remove the file if command is empty
        if file_path.exists() {
            fs::remove_file(&file_path)?;
        }
    }
    Ok(())
}

/// Parse a .desktop file and extract the application name and command
fn parse_desktop_file(path: &PathBuf) -> Option<(String, String)> {
    let content = fs::read_to_string(path).ok()?;
    let mut name: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut in_desktop_entry = false;
    
    for line in content.lines() {
        let line = line.trim();
        
        if line == "[Desktop Entry]" {
            in_desktop_entry = true;
            continue;
        } else if line.starts_with('[') {
            in_desktop_entry = false;
            continue;
        }
        
        if !in_desktop_entry {
            continue;
        }
        
        if let Some(value) = line.strip_prefix("Name=") {
            name = Some(value.to_lowercase().to_string());
        } else if let Some(value) = line.strip_prefix("Exec=") {
            // Remove field codes like %f, %F, %u, %U, etc.
            let cleaned = value
                .split_whitespace()
                .filter(|s| !s.starts_with('%'))
                .collect::<Vec<_>>()
                .join(" ");
            exec = Some(cleaned);
        }
        
        // Stop early if we have both
        if name.is_some() && exec.is_some() {
            break;
        }
    }
    
    match (name, exec) {
        (Some(n), Some(e)) if !e.is_empty() => Some((n, e)),
        _ => None,
    }
}

/// Get XDG desktop file directories in priority order
fn get_desktop_file_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    
    // User-specific directory (highest priority)
    if let Ok(data_home) = std::env::var("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data_home).join("applications"));
    } else if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    
    // System directories
    if let Ok(data_dirs) = std::env::var("XDG_DATA_DIRS") {
        for dir in data_dirs.split(':') {
            if !dir.is_empty() {
                dirs.push(PathBuf::from(dir).join("applications"));
            }
        }
    } else {
        // Default system directories
        dirs.push(PathBuf::from("/usr/local/share/applications"));
        dirs.push(PathBuf::from("/usr/share/applications"));
    }
    
    dirs
}

/// Prefix marking map keys derived from a desktop file id (filename stem), so
/// that id lookups never shadow or get confused with name-keyed entries.
const APP_ID_KEY_PREFIX: &str = "id:";

/// Key under which an application's desktop file id is indexed in the xdg map
pub fn xdg_id_key(app_id: &str) -> String {
    format!("{}{}", APP_ID_KEY_PREFIX, app_id.to_lowercase())
}

/// Create a map between application names and their commands from XDG .desktop files
/// Returns a HashMap where keys are application names (lowercased) and values are the
/// commands to run them. Each entry is also indexed by its desktop file id (filename
/// stem, lowercased) under `xdg_id_key`.
pub fn build_xdg_command_map() -> HashMap<String, String> {
    build_xdg_command_map_from(&get_desktop_file_dirs())
}

/// As `build_xdg_command_map`, but reading from the given directories (highest priority first)
pub fn build_xdg_command_map_from(dirs: &[PathBuf]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut ids: Vec<(String, String)> = Vec::new();

    for dir in dirs {
        if !dir.exists() {
            continue;
        }

        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();

                // Only process .desktop files
                if path.extension().and_then(|s| s.to_str()) != Some("desktop") {
                    continue;
                }

                if let Some((name, command)) = parse_desktop_file(&path) {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        ids.push((xdg_id_key(stem), command.clone()));
                    }
                    // Only add if not already present (respects priority order)
                    map.entry(name).or_insert(command);
                }
            }
        }
    }

    for (key, command) in ids {
        map.entry(key).or_insert(command);
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn test_fake_command() {
        let result = fake_command("firefox", "flatpak run org.mozilla.firefox");
        assert!(result.is_ok());
        assert!(std::path::PathBuf::from(std::env::var("HOME").unwrap())
            .join(".local/bin/firefox")
            .exists());
        let cleanup = fake_command("firefox", "");
        assert!(cleanup.is_ok());
        assert!(!std::path::PathBuf::from(std::env::var("HOME").unwrap())
            .join(".local/bin/firefox")
            .exists());
    }

    #[test]
    fn test_build_xdg_command_map() {
        let commands = build_xdg_command_map();
        
        // The map should not be empty on most systems
        // We can't test specific applications since they vary by system
        // But we can test the structure
        for (name, command) in commands.iter().take(5) {
            println!("Found: {} -> {}", name, command);
            assert!(!name.is_empty());
            assert!(!command.is_empty());
        }
    }

    #[test]
    fn test_parse_desktop_file() {
        // Create a temporary .desktop file for testing
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("test.desktop");
        
        let content = r#"[Desktop Entry]
Name=Test Application
Exec=test-app --flag %f
Type=Application
Icon=test-icon
"#;
        fs::write(&test_file, content).unwrap();
        
        let result = parse_desktop_file(&test_file);
        assert!(result.is_some());
        
        let (name, command) = result.unwrap();
        assert_eq!(name, "test application");
        assert_eq!(command, "test-app --flag");
        
        // Cleanup
        fs::remove_file(&test_file).unwrap();
    }

    #[test]
    fn test_xdg_map_indexes_by_desktop_id() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("dev.vencord.Vesktop.desktop"),
            "[Desktop Entry]\nName=Vesktop\nExec=vesktop-launcher %U\n",
        ).unwrap();
        // A stale entry whose Name collides with what the title step would look up
        fs::write(
            dir.path().join("discord-webapp.desktop"),
            "[Desktop Entry]\nName=Discord\nExec=chromium --app=https://discord.com\n",
        ).unwrap();

        let map = build_xdg_command_map_from(&[dir.path().to_path_buf()]);
        assert_eq!(map.get(&xdg_id_key("dev.vencord.vesktop")).map(String::as_str), Some("vesktop-launcher"));
        assert_eq!(map.get("vesktop").map(String::as_str), Some("vesktop-launcher"));
        assert_eq!(map.get("discord").map(String::as_str), Some("chromium --app=https://discord.com"));
        // Id keys never appear as plain name keys
        assert!(!map.contains_key("dev.vencord.vesktop"));
    }

    #[test]
    fn test_xdg_name_keys_win_over_id_keys() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.desktop"), "[Desktop Entry]\nName=b\nExec=one\n").unwrap();
        let map = build_xdg_command_map_from(&[dir.path().to_path_buf()]);
        assert_eq!(map.get("b").map(String::as_str), Some("one"));
        assert_eq!(map.get(&xdg_id_key("A")).map(String::as_str), Some("one"));
    }
}
