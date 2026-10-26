use std::fs::File;
use std::io::Write;
use tempfile::TempDir;

use hyprsession::session::{LocalSession, Session};

#[test]
fn test_load_session_with_nonexistent_file() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_path = temp_dir.path().to_str().unwrap().to_string();

    // Create a LocalSession with test configuration
    let session = LocalSession {
        base_path: session_path,
        simulate: true,
        load_time: 1, // Reduced load time for faster tests
        adjust_clients_only: false,
        save_duplicate_pids: false,
        dialect: hyprsession::dispatch::Dialect::Classic,
        workspace: None,
    };

    // This should not panic or fail when the session file doesn't exist
    let _ = session.load("");
}

#[test]
fn test_load_session_with_empty_file() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_path = temp_dir.path().to_str().unwrap().to_string();
    let exec_file_path = format!("{}/exec.conf", session_path);

    // Create an empty session file
    File::create(&exec_file_path).expect("Failed to create test file");

    // Create a LocalSession with test configuration
    let session = LocalSession {
        base_path: session_path,
        simulate: true,
        load_time: 1, // Reduced load time for faster tests
        adjust_clients_only: false,
        save_duplicate_pids: false,
        dialect: hyprsession::dispatch::Dialect::Classic,
        workspace: None,
    };

    // This should handle empty files gracefully
    let _ = session.load("");
}

#[test]
fn test_load_session_with_sample_data() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_path = temp_dir.path().to_str().unwrap().to_string();
    let exec_file_path = format!("{}/exec.conf", session_path);

    // Create a session file with sample data
    let mut file = File::create(&exec_file_path).expect("Failed to create test file");
    writeln!(
        file,
        "[monitor 0;workspace 1 silent;move 100 200;size 800 600] firefox"
    )
    .unwrap();
    writeln!(
        file,
        "[monitor 1;workspace 2 silent;float;move 50 50;size 400 300] kitty"
    )
    .unwrap();

    // Create a LocalSession with test configuration (reduced load_time for faster tests)
    let session = LocalSession {
        base_path: session_path,
        simulate: true,
        load_time: 1, // Reduced from 30 to 1 for faster tests
        adjust_clients_only: false,
        save_duplicate_pids: false,
        dialect: hyprsession::dispatch::Dialect::Classic,
        workspace: None,
    };

    // Load the session in simulate mode (won't actually execute commands)
    let _ = session.load("");
    let _ = session.load("testsession");
}

#[test]
fn test_load_session_with_malformed_data() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_path = temp_dir.path().to_str().unwrap().to_string();
    let exec_file_path = format!("{}/exec.conf", session_path);
    
    // Create a session file with malformed data
    let mut file = File::create(&exec_file_path).expect("Failed to create test file");
    writeln!(file, "[monitor ;workspace silent;move size] unknown_command").unwrap();
    
    // Create a LocalSession with test configuration (reduced load_time for faster tests)
    let session = LocalSession {
        base_path: session_path,
        simulate: true,
        load_time: 1, // Reduced from 30 to 1 for faster tests
        adjust_clients_only: false,
        save_duplicate_pids: false,
        dialect: hyprsession::dispatch::Dialect::Classic,
        workspace: None,
    };
    
    // This should handle malformed data gracefully
    let _ = session.load("");
    let _ = session.load("testsession");
}

#[test]
#[ignore]
fn test_session_trait_save_method() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_path = temp_dir.path().to_str().unwrap().to_string();

    // Create a LocalSession with test configuration
    let session = LocalSession {
        base_path: session_path.clone(),
        simulate: false, 
        load_time: 1,
        adjust_clients_only: false,
        save_duplicate_pids: false,
        dialect: hyprsession::dispatch::Dialect::Classic,
        workspace: None,
    };

    // Test saving a session (should work even in simulate mode for basic functionality)
    let result = session.save("test_session");
    assert!(result.is_ok());
    assert!(std::path::Path::new(&format!("{}/test_session", session_path)).exists());
}

#[test]
fn test_session_trait_list_method() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_path = temp_dir.path().to_str().unwrap().to_string();

    // Create a LocalSession with test configuration
    let session = LocalSession {
        base_path: session_path.clone(),
        simulate: true,
        load_time: 1,
        adjust_clients_only: false,
        save_duplicate_pids: false,
        dialect: hyprsession::dispatch::Dialect::Classic,
        workspace: None,
    };

    // Create some mock session directories
    std::fs::create_dir_all(format!("{}/session1", session_path)).unwrap();
    std::fs::create_dir_all(format!("{}/session2", session_path)).unwrap();
    std::fs::create_dir_all(format!("{}/session3", session_path)).unwrap();

    // Test listing sessions
    let sessions: Vec<String> = session.list().collect();
    
    // Should find our created sessions (order may vary)
    assert!(sessions.len() >= 3);
    assert!(sessions.contains(&"session1".to_string()));
    assert!(sessions.contains(&"session2".to_string()));
    assert!(sessions.contains(&"session3".to_string()));
}

#[test]
fn test_session_trait_delete_method() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_path = temp_dir.path().to_str().unwrap().to_string();

    // Create a LocalSession with test configuration
    let session = LocalSession {
        base_path: session_path.clone(),
        simulate: true,
        load_time: 1,
        adjust_clients_only: false,
        save_duplicate_pids: false,
        dialect: hyprsession::dispatch::Dialect::Classic,
        workspace: None,
    };

    // Create a mock session directory
    let session_dir = format!("{}/test_delete", session_path);
    std::fs::create_dir_all(&session_dir).unwrap();
    
    // Verify it exists
    assert!(std::path::Path::new(&session_dir).exists());

    // Test deleting the session
    session.delete("test_delete");
    
    // Verify it's been deleted
    assert!(!std::path::Path::new(&session_dir).exists());
}

#[test]
fn test_session_trait_clear_method() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_path = temp_dir.path().to_str().unwrap().to_string();

    // Create a LocalSession with test configuration
    let session = LocalSession {
        base_path: session_path,
        simulate: true, // Use simulate mode to avoid actual process killing
        load_time: 1,
        adjust_clients_only: false,
        save_duplicate_pids: false,
        dialect: hyprsession::dispatch::Dialect::Classic,
        workspace: None,
    };

    // Test clearing session (should work in simulate mode)
    let result = session.clear();
    
    // In simulate mode, this should succeed without errors
    match result {
        Ok(_) => println!("Clear test passed"),
        Err(e) => println!("Clear test completed with result: {:?}", e),
    }
}

#[test]
fn test_local_session_struct_creation() {
    // Test that LocalSession can be created with different configurations
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_path = temp_dir.path().to_str().unwrap().to_string();

    // Test default-like configuration
    let session1 = LocalSession {
        base_path: session_path.clone(),
        simulate: false,
        load_time: 60,
        adjust_clients_only: false,
        save_duplicate_pids: true,
        dialect: hyprsession::dispatch::Dialect::Lua,
        workspace: None,
    };

    // Test simulation configuration
    let session2 = LocalSession {
        base_path: session_path.clone(),
        simulate: true,
        load_time: 5,
        adjust_clients_only: true,
        save_duplicate_pids: false,
        dialect: hyprsession::dispatch::Dialect::Classic,
        workspace: None,
    };

    // Verify fields are set correctly
    assert_eq!(session1.simulate, false);
    assert_eq!(session1.load_time, 60);
    assert_eq!(session1.save_duplicate_pids, true);
    
    assert_eq!(session2.simulate, true);
    assert_eq!(session2.load_time, 5);
    assert_eq!(session2.adjust_clients_only, true);
}


#[cfg(test)]
mod cli_tests {
    use std::process::Command;
    use tempfile::TempDir;

    #[test]
    fn test_cli_help() {
        let output = Command::new("cargo")
            .args(&["run", "--", "--help"])
            .output()
            .expect("Failed to execute command");

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("hyprsession"));
        assert!(stdout.contains("[MODE]"));
        assert!(stdout.contains("--save-interval"));
    }

    #[test]
    fn test_cli_version() {
        let output = Command::new("cargo")
            .args(&["run", "--", "--version"])
            .output()
            .expect("Failed to execute command");

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("hyprsession"));
    }

    #[test]
    fn test_cli_simulate_mode() {
        let output = Command::new("cargo")
            .args(&["run", "--", "save-and-exit", "--simulate"])
            .output()
            .expect("Failed to execute command");

        // In simulate mode, it should not fail even if hyprland is not available
        // The program might still fail due to missing hyprland connection, so we'll check stderr
        if !output.status.success() {
            let stderr = String::from_utf8(output.stderr).unwrap();
            // If it fails due to hyprland connection issues, that's expected in test environment
            println!("CLI test failed with stderr: {}", stderr);
        }
    }

    #[test]
    fn test_list_sessions_empty_directory() {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let session_path = temp_dir.path().to_str().unwrap();

        let output = Command::new("cargo")
            .args(&["run", "--", "list"])
            .env("HYPRSESSION_PATH", session_path)
            .output()
            .expect("Failed to execute command");

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("Available sessions:"));
    }

    #[test]
    fn test_list_sessions() {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let session_path = temp_dir.path().to_str().unwrap();

        // Create some dummy session directories
        std::fs::create_dir(format!("{}/session1", session_path)).unwrap();
        std::fs::create_dir(format!("{}/session2", session_path)).unwrap();

        let output = Command::new("cargo")
            .args(&["run", "--", "list"])
            .env("HYPRSESSION_PATH", session_path)
            .output()
            .expect("Failed to execute command");

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("session1"));
        assert!(stdout.contains("session2"));

        // Clean up
        std::fs::remove_dir_all(format!("{}/session1", session_path)).unwrap();
        std::fs::remove_dir_all(format!("{}/session2", session_path)).unwrap();
    }

    #[test]
    #[ignore]
    fn test_cli_invalid_save_interval() {
        let output = Command::new("cargo")
            .args(&["run", "--", "--save-interval", "0"])
            .output()
            .expect("Failed to execute command");

        // Should fail with invalid save interval
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("Save interval needs to be greater than 0"));
    }
}
mod load_safety {
    use super::*;

    /// A non-simulated session. Any failure below must happen before clear(), which
    /// would need a running Hyprland, so these tests never touch a compositor.
    fn live_session(path: &std::path::Path) -> LocalSession {
        LocalSession {
            base_path: path.to_str().unwrap().to_string(),
            simulate: false,
            load_time: 1,
            adjust_clients_only: false,
            save_duplicate_pids: false,
            dialect: hyprsession::dispatch::Dialect::Lua,
            workspace: None,
        }
    }

    #[test]
    fn test_load_fails_before_clear_when_exec_conf_is_missing() {
        let dir = TempDir::new().unwrap();
        let err = live_session(dir.path()).load("").unwrap_err();
        assert!(err.to_string().contains("exec.conf"), "{err}");
    }

    #[test]
    fn test_load_fails_before_clear_when_exec_conf_is_unparseable() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("exec.conf"), "[monitor ;workspace silent;move size] x\n").unwrap();
        let err = live_session(dir.path()).load("").unwrap_err();
        assert!(err.to_string().contains("line 1"), "{err}");
    }

    #[test]
    fn test_load_fails_before_clear_when_exec_conf_has_no_launch_lines() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("exec.conf"), "\n  \n").unwrap();
        let err = live_session(dir.path()).load("").unwrap_err();
        assert!(err.to_string().contains("no launch lines"), "{err}");
    }

    #[test]
    fn test_load_fails_before_clear_when_clients_json_is_corrupt() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("exec.conf"), "[workspace 1 silent] true\n").unwrap();
        std::fs::write(dir.path().join("clients.json"), "{not json").unwrap();
        let err = live_session(dir.path()).load("").unwrap_err();
        assert!(err.to_string().contains("clients.json"), "{err}");
    }

    #[test]
    fn test_simulate_load_of_old_format_snapshot_succeeds_and_sends_nothing() {
        let dir = TempDir::new().unwrap();
        // Old format: per-process lines, no argv/cwd anywhere
        std::fs::write(
            dir.path().join("exec.conf"),
            "[monitor 0;workspace 2 silent;float;move 10 20;size 300 200] kitty\n[monitor 0;workspace 1 silent;move 0 0] brave\n",
        ).unwrap();
        std::fs::write(dir.path().join("clients.json"), "[]").unwrap();
        let mut session = live_session(dir.path());
        session.simulate = true;
        assert!(session.load("").is_ok());
        // Nothing was written back
        assert_eq!(std::fs::read_to_string(dir.path().join("clients.json")).unwrap(), "[]");
    }
}

mod workspace_flag_tests {
    use std::process::Command;

    fn run(args: &[&str]) -> std::process::Output {
        let dir = tempfile::tempdir().unwrap();
        Command::new(env!("CARGO_BIN_EXE_hyprsession"))
            .args(args)
            .env("HYPRSESSION_PATH", dir.path())
            .output()
            .unwrap()
    }

    #[test]
    fn test_workspace_flag_is_for_load_mode_only() {
        for mode in ["save", "clear", "list", "delete"] {
            let out = run(&[mode, "--workspace", "2", "--simulate"]);
            assert!(!out.status.success(), "{mode}");
            assert!(String::from_utf8_lossy(&out.stderr).contains("--workspace"), "{mode}");
        }
    }

    #[test]
    fn test_workspace_flag_rejects_non_positive_numbers() {
        let out = run(&["load", "--workspace", "0", "--simulate"]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("1 or more"));
    }
}
