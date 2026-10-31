# Hyprsession
## Overview
Implements session persistence for Hyprland. While the program is running it periodically saves the command, workspace and other properties of running clients found by `hyprctl clients`. These are then saved to a file formatted as a Hyprland config file which can then be sourced so that the session is restored when Hyprland is restarted.

<a href="https://www.buymeacoffee.com/joshurtree" target="_blank"><img src="https://cdn.buymeacoffee.com/buttons/default-red.png" alt="Buy Me A Coffee" height="41" width="174"></a>

## Installation
#### Manual
As root run the command 
```
cargo install --root /usr/local hyprsession
``` 
Or install as a user by replacing `/usr/local` with your home directory. 

#### Arch Linux
Hyprsession can be installed via the AUR. By either running your aur package manager of choice or manually by running
```
git clone https://aur.archlinux.org/hyprsession.git
cd hyprsession
makepkg -i
```
#### NixOS
Add the input to your `flake.nix`
```
hyprsession.url = "github:joshurtree/hyprsession"
```

Then either add the package to your `configuration.nix` or use `${inputs.hyprsession.packages.${pkgs.system}.hyprsession}/bin/hyprsession` in place of `hyprsession` to run the program.

## Usage
To automatically run the program in future sessions add the following line to your Hyprland config file (Usually at ~/.config/hypr/hyprland.conf)
```
exec-once = hyprsession
```
The same line can be added to your `home.nix` hyprland configuration if your are using Nix Home Manager.
If you want to save a session that is already running then run
```
hyprsession save <session-name>
```

## Options
Various options can be used to modify the behavior of Hyprsession.

### [mode]
One of
* default - Loads the session at startup the saves the current session at regular intervals.
* load - Load the session given by name (clears current session first, then launches each distinct command recorded in the snapshot (the saved argv; exec.conf for older snapshots): windows are asked to close, then any left are sent SIGTERM after 15s, then SIGKILL; the load aborts if anything survives)
* save - Save the current session 
* clear - Clear the current session (removes all windows)
* list - List all available sessions
* delete - Delete a session
* command - Create a command to deal with edge cases (see below)
 
### [name]
Name of the session or command

### [command]
Command to run (see below) 

### -l, --load-time <load_time>
How long, after the last program has been launched, windows keep being matched to their saved counterparts. Loading ends early once every saved window has been placed. Each program is given up to 20 seconds to show a window before the next is launched.

### -i, --save-interval <save_interval>
This sets the interval in seconds between session saves. The default is 60 seconds.

### --simulate
Only simulate loading and clearing of sessions

### --dialect <dialect>
Selects how dispatches are sent to Hyprland: `auto` (default), `lua` or `classic`.
`auto` probes the running compositor (under `--simulate` it guesses from the config files instead
and sends nothing). Use `lua` for Hyprland builds using the Lua config provider.
The dialect in use is printed when a session is loaded.

### --adjust-clients-only
When loading a session this tell Hyprsession to not clear the current session and restart programs. 
Instead it moves the existing clients to match the saved session.

### --workspace <workspace>
Only restore one workspace of the saved session (`hyprsession load <name> --workspace 3`).
Only the windows on that workspace are closed and only the programs behind its saved windows are
launched; other workspaces' windows are not moved, closed or focused. A window on that workspace
that shares its process with a window on another workspace is closed gracefully but never killed;
if it refuses to close it is reported. Works with `load` only.

### --mode <mode> (depreciated)
Sets the mode the program runs in 
* Default - Loads the session at startup the saves the current session at regular intervals.
* SaveOnly - As above but skips loading the session
* LoadAndExit - Load the saved session then immediately exit
* SaveAndExit - Save the current session then exit

### -h, --help
Display program help

### -v, --version
Display program version

## Command detection and bridge commands
Detecting what command is needed to create any given window in wayland does not have a single uniform solution.
The information within the `/proc` directory is used to try and find this. If this doesn't yield a command available 
via $PATH then the `initial_class` of the window is checked followed by `initial_title`.  

With applications that come in the form of Flatpaks, Snaps or Electron apps this method does not produce a proper result. 
To solve this requires the use of bridge commands. By running `hyprctl clients` you can find the `initial_class` of the window. You can then create a bridging command to run the correct command on startup. For example if you use the Firefox flatpak then running
```
hyprsession command firefox "flatpak run org.mozilla.firefox"
```
creates a script in the `~/.local/bin` directory that runs the correct command.

## Working directories
Each window is launched from the working directory it had when it was saved (`cd <dir> && <command>`), so programs
started with relative paths (`./my-app`) come back. If the directory no longer exists the program is launched from
the loader's directory and a warning names the window and the missing directory. Windows of sandboxed (Flatpak)
applications are launched through their desktop entry and are not wrapped.

## Programs running inside terminals
When a saved window is a terminal (currently `foot`, `kitty` and `alacritty`), hyprsession looks in `/proc` for the
program holding the terminal's foreground and records its command line and directory in `clients.json`
(the optional `foreground` field). On load the terminal is relaunched running that program, in its recorded
directory; when the program exits the terminal falls through to a shell prompt instead of closing. The terminal's own
recorded directory is that of its shell.

Which command is run is decided by program name:
* a program in the resume table is started with its resume command (below);
* any other program is replayed with its recorded command line (set `unlisted = "prompt"` to restore the terminal
  at a clean prompt instead);
* a program that owns its own window (for instance a GUI app started from the terminal, matched by process id)
  is not relaunched in the terminal: the terminal returns at a prompt and the program returns through its own window.

The built-in resume table (replacement command lines; any other flags that were recorded are dropped):

| Program | Resume command |
|---|---|
| claude | `claude --continue` |
| muse | `muse resume --last` |
| codex | `codex resume --last` |
| gemini | `gemini --resume` |
| aider | `aider --restore-chat-history` |
| opencode | `opencode --continue` |
| copilot | `copilot --continue` |
| goose | `goose session --resume` |
| qwen | `qwen --continue` |

`~/.config/hyprsession/resume.toml` (or `$XDG_CONFIG_HOME/hyprsession/resume.toml`) extends and overrides it:
```toml
[settings]
unlisted = "replay"      # or "prompt"

[resume]
claude = "claude --continue --model sonnet"
mytool = "mytool --last"
aider  = ""              # resume nothing: restore the terminal at a prompt
```
A resume file that cannot be parsed stops a load before anything is closed (the error names the file and line),
including under `--simulate`.

Note that the command lines of foreground programs are written to `clients.json`: do not keep secrets in
command-line arguments if snapshots are shared.

## Window counts and reports
Programs that own several saved windows are launched once. If fewer windows than were saved appear, the launch is
repeated (at most once per missing window); a shortfall that remains is reported with the saved windows left
unmatched, and the load still completes. After placement, live windows that no saved window claimed are listed
(address, class, title, workspace); they are never touched.

## Tiled window order
Dwindle builds its tree from the order windows are inserted, and each new window splits the leaf nearest the mouse
pointer. Placing each window as it maps would let app start-up latency and the pointer decide who sits where. A load
therefore claims tiled windows as they appear but places each workspace's tiled windows together, once every saved
tiled window of that workspace is claimed or its launches are over. For each workspace the loader rebuilds the saved
tree from the saved rectangles and inserts the windows in the order that regrows it, moving the mouse pointer into the
leaf to split before each insertion (a load already closes every window, so steering the pointer for its duration is
minor); the saved tree is reproduced whatever the pointer was, and the pointer is put back where it was when the load
finishes. Afterwards the loader reads the geometry back and says how many windows are at their saved geometry.
Floating windows are placed exactly as before.

If the compositor does not accept a pointer warp (checked by reading the pointer back), the order is computed for the
pointer as found; when that cannot regrow the tree (layout other than dwindle, `dwindle:force_split` other than 2, a
pointer exactly on a boundary) the workspace falls back to the saved reading order and the load says why. A tree that
fallback produces is still one the next load reproduces exactly. Windows destined for the workspace that is visible
when the load starts are not moved and keep their arrival position. Dragged (non-50/50) dividers are not restored.

## Loading survives its terminal
`hyprsession load` closes every window, including the terminal it was typed in. It therefore re-runs itself in its own
session (no controlling terminal, SIGHUP ignored), writing to `load.log` in the session directory, and the command you
typed just follows that log and returns the load's exit status: closing the terminal mid-load ends only the follower.
Set `HYPRSESSION_NO_DETACH=1` to run a load in the foreground.

## Change log
### Unreleased
* Place tiled windows per workspace so the saved tree regrows exactly: steered insertion order, pointer restored afterwards (computed order, then reading order, as fallbacks)
* A load detaches itself from its terminal and logs to `load.log`
* Launch windows from their saved working directory
* Restore the program running in the foreground of a terminal (resume table, `resume.toml`)
* Launch again when a program produced fewer windows than were saved
* `--workspace` to load a single workspace
* Report live windows that match no saved window
### 0.1.1
* Changed --session-path option to point at base directory of session file
### 0.1.2
* Fixed bug which would crash program if no session file existed
### 0.1.3
* Fix fatal bug on Hyprland 0.4 that crashed the program while saving the session  
### 0.1.4
* Changed fullscreen dispatcher to use fullscreenmode and fixed FullscreenMode type issue (#2)
* Updated to latest alpha version of hyprland crate. Fixes panic on fetching clients (#1)
### 0.1.5
* Skip saving clients with duplicate pids (i.e. generated by the same application)
* Add option to retain clients with duplicate pids
### 0.1.6
* Handle special workspaces properly
* Handle wrapped programs in Nixos
### 0.2.0
* Breaking change: Remove the need to use '--mode' to specify what mode to launch the program in
* Introduce named sessions
* Improve method of finding originating command for clients
* Track client creation to try and better place them
* Introduce bridge command functionality to deal with programs complicated launch patterns (flatpak, snap etc.)
* Refactor code to allow potential future use as either standalone or as a client
### 0.2.1
* Fix faulty post client creation logic
* Cleanup and properly document parameters
* Add parameter documentation test
* Use .desktop files to help find correct command

## Future development
The original intention for this project was to replicate the experience I had in KDE where most my programs would return after I switched on
my computer. This with this update (0.2.1) it seems to be able to do that but the way it is achieved is somewhat hacky. 
The problem is that the Hyprland dispatcher/event system is oriented around user interaction. This is particularly apparent with application 
grouping that has dispatchers for adding applications to a group on the left or right. For a user this is easy but the program does know 
its left from right, so I have to program that logic in as well and any method could be easily broken by edge cases or future Hyprland updates.

The plan going forward is to concentrate on porting the code to a Hyprland plugin. This should give me low level access to client positioning
needed to maintain the session in a cleaner manner and access new features without relying on a dispatcher being created to perform the task. 
The intention is the maintain the standalone program and fix any bugs that found. It will also become the cli frontend of the plugin. I am
not planning to add any further features to `hyprsession`. Instead enhancements such as grouping applications will come via the plugin.

## Development
### Running Tests
Along side the standard tests you can create a test within a vm using nix by running 
```bash
cd tests/vm-test
./run-test.sh
```

This will:
1. Build a NixOS VM with Hyprland and hyprsession
2. Start test applications (Firefox, terminal, calculator, etc.)
3. Save a session with `hyprsession save`
4. Clear the workspace and restore with `hyprsession load`  
5. Compare before/after states and report results

Test results are saved to `tests/test-results/` with detailed comparison data. The programs started can be changed by
editing `tests/vm-test/exec.conf`

### Session location
By default the sessions are saved to `~/.local/share/hyprsession`. For testing without disrupting your main instance you can set the variable `HYPRSESSION_PATH` variable in your shell before running the test version of the program. 

## Thanks

Thank you to the following people for helping to improve this project

* Tie C (ticia)
* Isaac Hesslegrave (HeadedBranch)
* Hornet (fsilly)