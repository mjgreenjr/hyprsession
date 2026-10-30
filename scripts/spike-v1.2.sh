#!/usr/bin/env bash
# v1.2 spike: how does dwindle place windows moved onto a workspace, and do ordered moves converge?
#
# Run on the real machine (Hyprland, Lua or classic dispatch dialect) from a checkout of main.
# Everything happens on ONE scratch workspace ($SPIKE_WS, default 9) using foot windows with
# distinct working directories. Nothing else on the desktop is closed or moved: the hyprsession
# loads are scoped (--workspace), and the prototype only touches windows it launched itself.
#
# Commands:  preflight | build | baseline | proto | proto-focus | export-fixture SNAP OUT | all | cleanup | selftest
# Spike #2 (split-target rule + computed insertion order; see docs/v1.2-plan.md, "Spike #2"):
#            rule | shape-insert NAME N | shape-hand NAME N | computed NAME [steer] | bar | spike2
# Unfloat variant (the user's own technique: float, hover, unfloat):  unfloat NAME [hidden|visible] | unfloat-bar
# See docs/v1.2-plan.md ("The spike") for what to run and what to paste back.
#
# Requires: hyprctl, jq, foot, a built hyprsession binary ($HYPRSESSION, default
# ./target/release/hyprsession). Snapshots go under $SPIKE_DIR (default /tmp/hs-spike) and contain
# your real window titles (hyprsession save is desktop-wide): they are never inside the repo, and
# `cleanup` deletes them.
set -euo pipefail

WS="${SPIKE_WS:-9}"                       # scratch workspace
COUNT="${SPIKE_N:-4}"                     # windows in the fixture (3-4)
ORDER="${SPIKE_ORDER:-}"                  # fixture build order, e.g. "1 3 2 4" (default 1..N)
BIN="${HYPRSESSION:-./target/release/hyprsession}"
DIR="${SPIKE_DIR:-/tmp/hs-spike}"
LOAD_TIME="${SPIKE_LOAD_TIME:-20}"        # hyprsession --load-time for the baseline loads
STEP_PAUSE="${SPIKE_STEP_PAUSE:-0.5}"     # seconds between prototype moves
ACTIVE_CASE="${SPIKE_ACTIVE_CASE:-0}"     # 1: the scratch workspace IS the visible one (moves are no-ops)
STAGE=0                                   # set by proto: 1 parks held windows on special:spike-stage first
FOCUS=0                                   # set by proto-focus: focus each window after moving it (DEC-B EXPERIMENT)
SESS="$DIR/sessions"
REPORT="$DIR/report.txt"
RUNS="$DIR/runs"
MODEL="$(cd "$(dirname "$0")" && pwd)/dwindle_model.py"
POINTER_MODE="${SPIKE_POINTER:-auto}"     # auto: try to warp the pointer, else ask you; manual: always ask; ignore: only log it
ALLOW_SWITCH="${SPIKE_ALLOW_SWITCH:-0}"   # unfloat visible mode switches to the scratch workspace and back (DEC-B relaxed, conditional)
ALLOW_POINTER="${SPIKE_ALLOW_POINTER:-0}" # computed ... steer warps the pointer before every move (new side effect: needs a ruling)

log() { printf '%s\n' "$*" | tee -a "$REPORT" >&2; }
die() { printf 'spike: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }

# ---------------------------------------------------------------- fingerprint (pure; see selftest)

# Six-field fingerprint of a snapshot's windows on workspace $2: class|workspace|floating|at|size|argv
fingerprint() {
  jq -r --argjson ws "$2" '.[] | select(.workspace.id == $ws)
      | [.class, .workspace.id, .floating, (.at | @json), (.size | @json), (.argv | @json)] | join("|")' "$1" | sort
}

# Print "EMPTY" or the diff of two snapshots' fingerprints on workspace $3
diff_report() {
  local a="$1" b="$2" ws="$3" out
  out="$(diff <(fingerprint "$a" "$ws") <(fingerprint "$b" "$ws") || true)"
  if [ -z "$out" ]; then echo "EMPTY"; else echo "$out"; fi
}

# Tiled windows of a snapshot on workspace $2 in saved reading order (x, then y):
# one "x y <argv joined by spaces>" per line
reading_order() {
  jq -r --argjson ws "$2" '[.[] | select(.workspace.id == $ws and .floating == false and .argv != null)]
      | sort_by(.at[0], .at[1]) | .[] | "\(.at[0]) \(.at[1]) \(.argv | join(" "))"' "$1"
}

selftest() {
  need jq
  local t; t="$(mktemp -d)"
  cat >"$t/a.json" <<'EOF'
[
 {"class":"foot","workspace":{"id":9},"floating":false,"at":[10,10],"size":[500,500],"argv":["foot","--working-directory=/x/d1"]},
 {"class":"foot","workspace":{"id":9},"floating":false,"at":[510,10],"size":[500,500],"argv":["foot","--working-directory=/x/d2"]},
 {"class":"foot","workspace":{"id":9},"floating":false,"at":[10,520],"size":[500,500],"argv":["foot","--working-directory=/x/d3"]},
 {"class":"mpv","workspace":{"id":2},"floating":false,"at":[0,0],"size":[1,1],"argv":["mpv"]}
]
EOF
  # b: same windows, d3 and d2 dealt differently; plus an unrelated window on another workspace
  sed 's#"at":\[510,10\],"size":\[500,500\]#"at":[510,10],"size":[500,1000]#' "$t/a.json" >"$t/b.json"
  [ "$(diff_report "$t/a.json" "$t/a.json" 9)" = "EMPTY" ] || die "selftest: identical snapshots must diff EMPTY"
  diff_report "$t/a.json" "$t/b.json" 9 | grep -q '500,1000' || die "selftest: geometry change must show in the diff"
  [ "$(diff_report "$t/a.json" "$t/b.json" 2)" = "EMPTY" ] || die "selftest: other workspaces must be ignored"
  [ "$(reading_order "$t/a.json" 9 | head -1)" = "10 10 foot --working-directory=/x/d1" ] || die "selftest: reading order"
  [ "$(reading_order "$t/a.json" 9 | sed -n 2p)" = "10 520 foot --working-directory=/x/d3" ] || die "selftest: reading order is x then y"
  rm -rf "$t"
  if command -v python3 >/dev/null 2>&1; then
    PYTHONDONTWRITEBYTECODE=1 python3 -I "$(dirname "$0")/test_dwindle_model.py" >/dev/null 2>&1 || die "selftest: dwindle_model tests failed (run scripts/test_dwindle_model.py)"
  fi
  echo "selftest ok"
}

# ---------------------------------------------------------------- compositor access

DIALECT=""
detect_dialect() {
  [ -n "$DIALECT" ] && return
  if [ "$(hyprctl dispatch 'submap reset' 2>&1 | tr -d '[:space:]')" = "ok" ]; then
    DIALECT=classic
  elif [ "$(hyprctl dispatch "hl.dsp.exec_cmd('true')" 2>&1 | tr -d '[:space:]')" = "ok" ]; then
    DIALECT=lua
  else
    die "the compositor accepted neither the classic nor the Lua dispatch probe"
  fi
}

dsp_exec() { # command (no single quotes)
  detect_dialect
  if [ "$DIALECT" = lua ]; then hyprctl dispatch "hl.dsp.exec_cmd('$1')" >/dev/null
  else hyprctl dispatch exec "$1" >/dev/null; fi
}
dsp_move() { # address workspace-or-special:name   (silent: the visible workspace does not follow)
  detect_dialect
  if [ "$DIALECT" = lua ]; then
    local target="$2"
    case "$2" in *[!0-9]*) target="'$2'" ;; esac
    hyprctl dispatch "hl.dsp.window.move({ workspace = $target, window = 'address:$1', follow = false })" >/dev/null
  else hyprctl dispatch movetoworkspacesilent "$2,address:$1" >/dev/null; fi
}
dsp_focus() { # address  (switches the visible workspace if the window is elsewhere: experiment only)
  detect_dialect
  if [ "$DIALECT" = lua ]; then hyprctl dispatch "hl.dsp.focus({ window = 'address:$1' })" >/dev/null
  else hyprctl dispatch focuswindow "address:$1" >/dev/null; fi
}
dsp_workspace() { # id
  detect_dialect
  if [ "$DIALECT" = lua ]; then hyprctl dispatch "hl.dsp.focus({ workspace = $1 })" >/dev/null
  else hyprctl dispatch workspace "$1" >/dev/null; fi
}
dsp_close() { # address
  detect_dialect
  if [ "$DIALECT" = lua ]; then hyprctl dispatch "hl.dsp.window.close({ window = 'address:$1' })" >/dev/null
  else hyprctl dispatch closewindow "address:$1" >/dev/null; fi
}

clients_on() { hyprctl clients -j | jq -c --argjson ws "$1" '[.[] | select(.workspace.id == $ws and .mapped)]'; }
all_addresses() { hyprctl clients -j | jq -r '.[] | select(.mapped) | .address' | sort; }
active_ws() { hyprctl activeworkspace -j | jq -r '.id'; }
is_fixture() { # pid -> success if the window is one of this script's foot fixture windows
  { tr '\0' ' ' <"/proc/$1/cmdline"; } 2>/dev/null | grep -qF -- "--working-directory=$DIR/cwd/"
}
# Refuse to touch a scratch workspace that holds anything but this script's own fixture windows
fixture_only_or_die() {
  local pid
  for pid in $(clients_on "$WS" | jq -r '.[].pid'); do
    is_fixture "$pid" || die "workspace $WS holds a window that is not a spike fixture window (pid $pid); refusing to close or reload over it. Use an empty scratch workspace (SPIKE_WS=N)."
  done
}
tag() { # short label of a window for tables: the fixture directory of its foot
  local pid="$1"
  { tr '\0' ' ' <"/proc/$pid/cmdline"; } 2>/dev/null | sed -n 's#.*working-directory=\([^ ]*\).*#\1#p' | sed 's#.*/##'
}

dump_ws() { # label
  log "  -- ws $WS: $1"
  clients_on "$WS" | jq -r '.[] | "\(.pid)\t\(.at | @json)\t\(.size | @json)\t\(.floating)"' |
    while IFS=$'\t' read -r pid at size floating; do
      printf '     %-4s at=%-12s size=%-12s floating=%s\n' "$(tag "$pid")" "$at" "$size" "$floating" | tee -a "$REPORT" >&2
    done
}

wait_for_count() { # workspace count timeout_s
  local i
  for i in $(seq 1 $(($3 * 4))); do
    [ "$(clients_on "$1" | jq 'length')" -ge "$2" ] && return 0
    sleep 0.25
  done
  return 1
}

wait_for_new() { # file-of-addresses-before timeout_s  -> prints the new address
  local i new
  for i in $(seq 1 $(($2 * 4))); do
    new="$(comm -13 "$1" <(all_addresses) | head -1)"
    if [ -n "$new" ]; then echo "$new"; return 0; fi
    sleep 0.25
  done
  return 1
}

close_ws() {
  fixture_only_or_die
  local a
  for a in $(clients_on "$WS" | jq -r '.[].address'); do dsp_close "$a"; done
  wait_for_empty
}
wait_for_empty() {
  local i
  for i in $(seq 1 40); do
    [ "$(clients_on "$WS" | jq 'length')" -eq 0 ] && return 0
    sleep 0.25
  done
  die "windows on workspace $WS did not close"
}

hs() { HYPRSESSION_PATH="$SESS" "$BIN" "$@"; }
save() { hs save "$1" >"$DIR/save-$1.log" 2>&1 || die "hyprsession save $1 failed; see $DIR/save-$1.log"; }
load() { # name  -> logs the workspace-moves the loader sent, in order
  hs load "$1" --workspace "$WS" --load-time "$LOAD_TIME" >"$DIR/load-$1.log" 2>&1 || die "hyprsession load $1 failed; see $DIR/load-$1.log"
  log "  moves sent by 'load $1' (in order):"
  grep -E "Sending: dispatch hl\.dsp\.window\.move\(\{ workspace|Sending: dispatch .*MoveToWorkspace" "$DIR/load-$1.log" |
    sed 's/^/     /' | tee -a "$REPORT" >&2 || log "     (none)"
}

# ---------------------------------------------------------------- commands

preflight() {
  need hyprctl; need jq; need foot
  [ -x "$BIN" ] || die "build hyprsession first (cargo build --release), or set HYPRSESSION"
  mkdir -p "$DIR" "$SESS"
  : >"$REPORT"
  log "== preflight ($(date -Is))"
  log "hyprland: $(hyprctl version -j | jq -r '.tag // .version // "unknown"')"
  log "hyprsession: $("$BIN" --version 2>&1)  ($(git rev-parse --short HEAD 2>/dev/null || echo 'no git'))"
  log "layout (DEC-C): $(hyprctl getoption general:layout -j | jq -c '{str, int}' 2>/dev/null || hyprctl getoption general:layout | tr '\n' ' ')"
  local opt
  for opt in dwindle:use_active_for_splits dwindle:force_split dwindle:preserve_split dwindle:smart_split dwindle:split_width_multiplier; do
    log "$opt: $(hyprctl getoption "$opt" -j 2>/dev/null | jq -c '{int, float, str}' 2>/dev/null || echo unavailable)   [plain: $(hyprctl getoption "$opt" 2>&1 | tr '\n' ' ')]"
  done
  log "workspace rules: $(hyprctl workspacerules -j 2>/dev/null | jq -c '.' 2>/dev/null || echo unavailable)"
  detect_dialect; log "dispatch dialect: $DIALECT"
  local active; active="$(active_ws)"
  log "active workspace: $active   scratch workspace: $WS   active-case: $ACTIVE_CASE"
  [ "$(clients_on "$WS" | jq 'length')" -eq 0 ] || die "scratch workspace $WS has windows; pick an empty one (SPIKE_WS=N)"
  if [ "$ACTIVE_CASE" = 1 ]; then
    [ "$active" = "$WS" ] || die "active-case needs the scratch workspace to be the visible one (switch to workspace $WS first)"
  else
    [ "$active" != "$WS" ] || die "workspace $WS is the visible one; switch to an empty other workspace first (or set SPIKE_ACTIVE_CASE=1)"
    [ "$(clients_on "$active" | jq 'length')" -eq 0 ] || die "the visible workspace ($active) must be empty: new windows land on it before being moved"
  fi
  log "preflight ok"
}

build() {
  preflight
  log "== build: $COUNT foot windows, moved to workspace $WS one at a time"
  local k n addr before
  [ -n "$ORDER" ] || ORDER="$(seq -s' ' 1 "$COUNT")"
  mkdir -p "$DIR/cwd"
  for k in $(seq 1 "$COUNT"); do mkdir -p "$DIR/cwd/d$k"; done
  before="$DIR/before.txt"
  for n in $ORDER; do
    all_addresses >"$before"
    dsp_exec "foot --working-directory=$DIR/cwd/d$n"
    addr="$(wait_for_new "$before" 20)" || die "foot d$n did not appear"
    [ "$ACTIVE_CASE" = 1 ] || dsp_move "$addr" "$WS"
    sleep 0.5
  done
  wait_for_count "$WS" "$COUNT" 10 || die "expected $COUNT windows on workspace $WS"
  dump_ws "fixture as built (order: $ORDER)"
  save spike-a
  log "saved snapshot spike-a; tiled windows in saved reading order:"
  reading_order "$SESS/spike-a/clients.json" "$WS" | sed 's/^/     /' | tee -a "$REPORT" >&2
}

baseline() {
  [ -f "$SESS/spike-a/clients.json" ] || die "run build first"
  need hyprctl; need jq
  fixture_only_or_die   # the scoped loads below close everything on the scratch workspace
  log "== baseline: current code, save A -> load A -> save B -> load B -> save C"
  load spike-a;  dump_ws "after load A";  save spike-b
  load spike-b;  dump_ws "after load B";  save spike-c
  log "baseline diff(A,B): $(diff_report "$SESS/spike-a/clients.json" "$SESS/spike-b/clients.json" "$WS")"
  log "baseline diff(B,C): $(diff_report "$SESS/spike-b/clients.json" "$SESS/spike-c/clients.json" "$WS")"
}

# Prototype of ordered placement, done from outside hyprsession: launch every tiled window of
# snapshot $1 so that it lands on the visible workspace (as a loader launch does), in arrival
# order $3, hold them there, then move them onto the scratch workspace in saved reading order.
proto_cycle() { # src-snapshot dst-snapshot arrival(forward|reverse|saved)
  local src="$SESS/$1/clients.json" dst="$2" arrival="$3"
  local lines=() ordered=() line addr before="$DIR/before.txt"
  mapfile -t lines < <(reading_order "$src" "$WS")
  [ "${#lines[@]}" -gt 0 ] || die "snapshot $1 has no tiled windows on workspace $WS"
  case "$arrival" in
    forward) ordered=("${lines[@]}") ;;
    reverse) for ((i = ${#lines[@]} - 1; i >= 0; i--)); do ordered+=("${lines[i]}"); done ;;
    saved)   mapfile -t ordered < <(jq -r --argjson ws "$WS" '.[] | select(.workspace.id == $ws and .floating == false and .argv != null)
                 | "\(.at[0]) \(.at[1]) \(.argv | join(" "))"' "$src") ;;
    *) die "unknown arrival order $arrival" ;;
  esac
  close_ws
  local held=() # "address<TAB>reading-index"
  for line in "${ordered[@]}"; do
    all_addresses >"$before"
    dsp_exec "${line#* * }"
    addr="$(wait_for_new "$before" 20)" || die "window for '${line#* * }' did not appear"
    # position of this window in saved reading order
    local idx=0 i
    for i in "${!lines[@]}"; do [ "${lines[i]}" = "$line" ] && idx=$i; done
    held+=("$addr	$idx")
    [ "$STAGE" = 1 ] && dsp_move "$addr" "special:spike-stage"
    sleep 0.3
  done
  log "  arrival order ($arrival, stage=$STAGE, focus=$FOCUS): ${#held[@]} windows launched and held on $([ "$STAGE" = 1 ] && echo special:spike-stage || echo "workspace $(active_ws)")"
  local start_ws; start_ws="$(active_ws)"
  log "  flushing in saved reading order, $STEP_PAUSE s apart:"
  local sorted
  sorted="$(printf '%s\n' "${held[@]}" | sort -t$'\t' -k2,2n)"
  while IFS=$'\t' read -r addr idx; do
    [ "$ACTIVE_CASE" = 1 ] || dsp_move "$addr" "$WS"
    [ "$FOCUS" = 1 ] && dsp_focus "$addr"
    sleep "$STEP_PAUSE"
    dump_ws "after moving reading-order #$idx (${lines[idx]#* * })"
  done <<<"$sorted"
  [ "$FOCUS" = 1 ] && dsp_workspace "$start_ws"
  sleep 1
  save "$dst"
}

proto() {
  [ -f "$SESS/spike-a/clients.json" ] || die "run build first"
  preflight_light
  log "== proto: ordered moves from outside hyprsession, two cycles per variant (step pause $STEP_PAUSE s)"
  local arrival stage
  for stage in 0 1; do
    for arrival in forward reverse; do
      STAGE=$stage
      local name="proto-$arrival-s$stage"
      log "-- variant: arrival=$arrival stage=$stage"
      proto_cycle spike-a "$name-b" "$arrival"
      log "-- second cycle (arrival=$arrival stage=$stage)"
      proto_cycle "$name-b" "$name-c" "$arrival"
      log "proto[$arrival,stage=$stage] diff(A,B'): $(diff_report "$SESS/spike-a/clients.json" "$SESS/$name-b/clients.json" "$WS")"
      log "proto[$arrival,stage=$stage] diff(B',C'): $(diff_report "$SESS/$name-b/clients.json" "$SESS/$name-c/clients.json" "$WS")"
    done
  done
  STAGE=0
  log "proto: forward vs reverse arrival, first cycle (queue is effective if EMPTY): $(diff_report "$SESS/proto-forward-s0-b/clients.json" "$SESS/proto-reverse-s0-b/clients.json" "$WS")"
  log "proto: staged vs unstaged, forward first cycle (staging is equivalent if EMPTY): $(diff_report "$SESS/proto-forward-s0-b/clients.json" "$SESS/proto-forward-s1-b/clients.json" "$WS")"
}

# DEC-B EXPERIMENT, opt-in only: like proto, but focuses each window after moving it, which
# switches the visible workspace to the scratch one (flicker) and back at the end. It only tells
# us what focus-steered insertion would buy; the v1.2 design does not use it.
proto_focus() {
  [ "${SPIKE_ALLOW_FOCUS:-0}" = 1 ] || die "proto-focus switches your visible workspace; set SPIKE_ALLOW_FOCUS=1 to run it"
  [ -f "$SESS/spike-a/clients.json" ] || die "run build first"
  preflight_light
  FOCUS=1; STAGE=0
  log "== proto-focus (EXPERIMENT; visible workspace flickers)"
  proto_cycle spike-a "focus-b" forward
  proto_cycle focus-b "focus-c" forward
  log "focus[forward] diff(A,B'): $(diff_report "$SESS/spike-a/clients.json" "$SESS/focus-b/clients.json" "$WS")"
  log "focus[forward] diff(B',C'): $(diff_report "$SESS/focus-b/clients.json" "$SESS/focus-c/clients.json" "$WS")"
  FOCUS=0
}

# Write the scratch workspace's slice of a snapshot as a synthetic, commit-safe fixture:
# only the fixture windows (foot with /tmp/hs-spike paths), geometry and argv, nothing else.
export_fixture() { # snapshot-name out-file
  [ -f "$SESS/$1/clients.json" ] || die "no snapshot $1"
  jq --argjson ws "$WS" '[.[] | select(.workspace.id == $ws and .argv != null and (.argv[0] == "foot"))]' "$SESS/$1/clients.json" >"$2"
  echo "wrote $2 ($(jq length "$2") windows); review it before committing" >&2
}

preflight_light() {
  need hyprctl; need jq; [ -x "$BIN" ] || die "hyprsession binary not found"
  local active; active="$(active_ws)"
  [ -n "${START_WS:-}" ] || START_WS="$active"
  if [ "$ACTIVE_CASE" != 1 ]; then
    if [ "$active" = "$WS" ] || [ "$(clients_on "$active" | jq 'length')" -ne 0 ]; then
      local staging="" cand
      for cand in 13 14 15 16 17 18; do
        [ "$cand" = "$WS" ] && continue
        [ "$(clients_on "$cand" | jq 'length')" -eq 0 ] && { staging="$cand"; break; }
      done
      [ -n "$staging" ] || die "no empty staging workspace found (tried 13-18)"
      log "  preflight: workspace $active is not a usable landing pad; switching you to empty workspace $staging for this stage"
      dsp_workspace "$staging"; sleep 0.5
    fi
  fi
}

cleanup() {
  need hyprctl; need jq
  # close only this script's fixture windows on the scratch workspace, never anything else
  local pid a
  if [ -d "$DIR" ]; then
    for a in $(clients_on "$WS" | jq -r '.[] | "\(.pid) \(.address)"' | while read -r pid a; do is_fixture "$pid" && echo "$a"; done); do dsp_close "$a"; done
  fi
  if [ "${KEEP:-0}" = 1 ]; then log "kept $DIR (it contains your real window titles)"; else rm -rf "$DIR"; echo "removed $DIR" >&2; fi
}

all() {
  build
  baseline
  proto
  log "== done. Paste $REPORT back (it lists no window titles, only fixture directories)."
}

# ================================================================ spike #2
# Pin the split-target rule, then test computed insertion orders. Nothing here uses hyprsession's
# loader: windows are launched (they land on the visible workspace), held, and moved onto the
# scratch workspace one at a time, exactly as the ordered-placement prototype does.

ptr() { hyprctl cursorpos -j 2>/dev/null | jq -r '"\(.x),\(.y)"' 2>/dev/null || echo "?"; }
active_win() { hyprctl activewindow -j 2>/dev/null | jq -r 'if .address then "\(.address)@\(.workspace.id)" else "none" end' 2>/dev/null || echo "?"; }
near() { awk -v g="$1" -v w="$2" 'BEGIN{split(g,a,",");split(w,b,",");dx=a[1]-b[1];dy=a[2]-b[2];exit !(dx*dx+dy*dy<=25)}'; }

# Quarter points of the focused monitor: TL TR BL BR, and C for the centre
pointer_target() { # name -> "x y"
  hyprctl monitors -j | jq -r --arg a "$1" '.[] | select(.focused) |
    (.x) as $x | (.y) as $y | (.width / .scale) as $w | (.height / .scale) as $h |
    ({"TL":[0.25,0.25],"TR":[0.75,0.25],"BL":[0.25,0.75],"BR":[0.75,0.75],"C":[0.5,0.5]}[$a]) as $f |
    "\(($x + $w * $f[0]) | floor) \(($y + $h * $f[1]) | floor)"'
}

warp_pointer() { # x y -> success if the pointer is now there
  detect_dialect
  if [ "$DIALECT" = classic ]; then hyprctl dispatch movecursor "$1 $2" >/dev/null 2>&1 || true
  else hyprctl dispatch "hl.dsp.cursor.move({ x = $1, y = $2 })" >/dev/null 2>&1 || true; fi
  sleep 0.2
  near "$(ptr)" "$1,$2"
}

in_area() { # NAME -> true when the pointer is parked in that screen area
  local area="$1" geo p mx my mw mh px py rx ry cx cy
  geo="$(hyprctl monitors -j | jq -r '.[] | select(.focused) | "\(.x) \(.y) \(((.width / .scale) | floor)) \(((.height / .scale) | floor))"')"
  p="$(ptr)"; px="${p%,*}"; py="${p#*,}"
  set -- $geo; mx=$1; my=$2; mw=$3; mh=$4
  rx=$((px-mx)); ry=$((py-my)); cx=$((mw/2)); cy=$((mh/2))
  case "$area" in
    TL) [ "$rx" -lt $((cx-30)) ] && [ "$ry" -lt $((cy-30)) ] ;;
    TR) [ "$rx" -gt $((cx+30)) ] && [ "$ry" -lt $((cy-30)) ] ;;
    BL) [ "$rx" -lt $((cx-30)) ] && [ "$ry" -gt $((cy+30)) ] ;;
    BR) [ "$rx" -gt $((cx+30)) ] && [ "$ry" -gt $((cy+30)) ] ;;
    C)  [ "$rx" -gt $((cx-150)) ] && [ "$rx" -lt $((cx+150)) ] && [ "$ry" -gt $((cy-150)) ] && [ "$ry" -lt $((cy+150)) ] ;;
  esac
}

park_pointer() { # TL|TR|BL|BR|C
  [ "$POINTER_MODE" = ignore ] && { log "  pointer: left where it is ($(ptr))"; return 0; }
  local xy x y how=manual
  xy="$(pointer_target "$1")"; x="${xy% *}"; y="${xy#* }"
  if [ "$POINTER_MODE" = auto ] && warp_pointer "$x" "$y"; then how="warped ($DIALECT dialect)"
  else
    printf 'spike: put the mouse pointer in the %s part of the screen (around %s,%s); waiting for it to settle there\n' "$1" "$x" "$y" >&2
    local where="$1"
    case "$1" in TL) where="top-left";; TR) where="top-right";; BL) where="bottom-left";; BR) where="bottom-right";; C) where="center";; esac
    command -v notify-send >/dev/null 2>&1 && notify-send -a spike "spike: park pointer $where" "anywhere in the $where of the screen - hold it there a second and the run continues" || true
    local tries=0 stable=0
    while [ "$tries" -lt 240 ]; do
      if in_area "$1"; then
        stable=$((stable+1)); [ "$stable" -ge 2 ] && { how="parked (polled)"; break; }
      else stable=0; fi
      tries=$((tries+1)); sleep 0.5
    done
    [ "$how" = manual ] && how="park TIMEOUT (pointer at $(ptr))"
  fi
  log "  pointer parked at $1 target $x,$y -> now $(ptr) [$how]"
}

table_tsv() { # tag x y w h for every window on the scratch workspace
  local pid x y w h
  clients_on "$WS" | jq -r '.[] | "\(.pid)\t\(.at[0])\t\(.at[1])\t\(.size[0])\t\(.size[1])"' |
    while IFS=$'\t' read -r pid x y w h; do printf '%s\t%s\t%s\t%s\t%s\n' "$(tag "$pid")" "$x" "$y" "$w" "$h"; done
}

# Insert N fixture windows (d1..dN) onto the scratch workspace one at a time, in ORDER (fixture
# numbers), recording the window table, pointer and active window around every move.
# The windows are launched first, in ARRIVAL order, and held on the visible workspace
# (or on special:spike-stage when STAGE=1).
probe_run() { # label n "order" [arrival] [stage]
  local label="$1" n="$2" order="$3" arrival="${4:-forward}" stage="${5:-0}"
  local rd="$RUNS/$label" k a step=0 start_ws discounted="" before="$DIR/before.txt"
  rm -rf "$rd"; mkdir -p "$rd" "$DIR/cwd"
  close_ws
  for k in $(seq 1 "$n"); do mkdir -p "$DIR/cwd/d$k"; done
  start_ws="$(active_ws)"
  declare -A addr_of=()
  local launch
  if [ "$arrival" = reverse ]; then launch="$(seq "$n" -1 1)"; else launch="$(seq 1 "$n")"; fi
  for k in $launch; do
    all_addresses >"$before"
    dsp_exec "foot --working-directory=$DIR/cwd/d$k"
    a="$(wait_for_new "$before" 20)" || die "foot d$k did not appear"
    addr_of[$k]="$a"
    [ "$stage" = 1 ] && dsp_move "$a" "special:spike-stage"
    sleep 0.3
  done
  # the held windows must not be on the scratch workspace yet, and the visible workspace must not have moved
  [ "$(clients_on "$WS" | jq 'length')" -eq 0 ] || discounted="windows landed directly on workspace $WS"
  [ "$(active_ws)" = "$start_ws" ] || discounted="visible workspace changed during launch"
  [ -z "$discounted" ] || log "  !! run $label discounted: $discounted"
  local pointers=() tags=() p
  for k in $order; do
    step=$((step + 1))
    p="$(ptr)"
    [ "$step" -ge 2 ] && pointers+=("$p")
    log "  [$label] step $step: insert d$k  pointer=$p  active=$(active_win)"
    dsp_move "${addr_of[$k]}" "$WS"
    sleep "$STEP_PAUSE"
    table_tsv >"$rd/t$step.tsv"
    tags+=("d$k")
  done
  jq -n --arg label "$label" --arg arrival "$arrival" --argjson stage "$stage" --arg discounted "$discounted" \
        --arg order "${tags[*]}" --arg pointers "${pointers[*]:-}" '
    {label: $label, arrival: $arrival, stage: $stage, order: ($order | split(" ")),
     pointers: ($pointers | split(" ") | map(select(. != "?") | split(",") | map(tonumber)))}
    + (if $discounted != "" then {discounted: true, reason: $discounted} else {} end)' >"$rd/meta.json"
  python3 "$MODEL" derive "$rd" | tee -a "$REPORT" >&2
  unset addr_of
}

rule_runs() {
  need hyprctl; need jq; need python3; need foot
  [ -x "$BIN" ] || die "hyprsession binary not found"
  mkdir -p "$DIR" "$SESS" "$RUNS"
  preflight_light
}

# Spike #2 part A: pin the rule with targeted fixtures
rule() {
  rule_runs
  : >"$REPORT"; : >"$RUNS/rule.jsonl"
  log "== rule ($(date -Is)); hyprland $(hyprctl version -j | jq -r '.tag // .version'); step pause $STEP_PAUSE s; pointer mode $POINTER_MODE"
  log "monitors: $(hyprctl monitors -j | jq -c '[.[] | {name, x, y, width, height, focused}]')"
  local area rep arr st n
  log "-- Block P: pointer matrix (5 windows, forward arrival, inserted 1..5)"
  for area in BL TR TL BR C; do park_pointer "$area"; probe_run "P-$area" 5 "1 2 3 4 5"; done
  log "-- Block A: arrival x staging at a fixed pointer, two repeats (the spike #1 anomaly cell is reverse/stage 1)"
  park_pointer BL
  for rep in 1 2; do for arr in forward reverse; do for st in 0 1; do
    probe_run "A-$arr-s$st-r$rep" 5 "1 2 3 4 5" "$arr" "$st"
  done; done; done
  log "-- Block N: window count 2,3,4,6 at two pointer areas"
  for area in BL TR; do park_pointer "$area"; for n in 2 3 4 6; do probe_run "N-$area-$n" "$n" "$(seq -s' ' 1 "$n")"; done; done
  log "-- Block R: repeatability (identical runs, pointer untouched)"
  park_pointer BL
  for rep in 1 2 3; do probe_run "R-BL-$rep" 4 "1 2 3 4"; done
  log "-- Block O: permuted insertion order (identities differ, count equal)"
  probe_run "O-perm" 5 "3 1 5 2 4"
  log "== hypothesis scores (a rule is confirmed when it matches every non-discounted run)"
  python3 "$MODEL" score "$RUNS/rule.jsonl" | tee -a "$REPORT" >&2
  python3 "$MODEL" score "$RUNS/rule.jsonl" | sed -n 's/^BEST //p' >"$DIR/rule.best"
  log "== rule done. Paste $REPORT and $RUNS/rule.jsonl back."
  command -v notify-send >/dev/null 2>&1 && notify-send -a spike "spike: rule done" "report and rule.jsonl are ready to paste back" || true
}

# A tiled shape built by insertion (reachable under the measured rule at the current pointer)
shape_insert() { # name n
  rule_runs
  local name="$1" n="$2"
  park_pointer BL
  probe_run "shape-$name" "$n" "$(seq -s' ' 1 "$n")"
  save "shape-$name"
  log "shape $name: $n windows built by insertion, saved as shape-$name"
}

# A tiled shape arranged by hand on the VISIBLE scratch workspace (any keybinds), as a real desk grows
shape_hand() { # name n
  need hyprctl; need jq; need foot
  local name="$1" n="$2" k before="$DIR/before.txt"
  mkdir -p "$DIR" "$SESS" "$RUNS" "$DIR/cwd"
  if [ "$(clients_on "$WS" | jq 'length')" -ne 0 ]; then
    log "  workspace $WS still holds the previous stage's windows; clearing them (fixtures only)"
    close_ws
  fi
  local home_ws="${START_WS:-$(active_ws)}"
  dsp_workspace "$WS"; sleep 0.5
  [ "$(active_ws)" = "$WS" ] || die "could not switch to workspace $WS"
  for k in $(seq 1 "$n"); do
    mkdir -p "$DIR/cwd/d$k"
    all_addresses >"$before"; dsp_exec "foot --working-directory=$DIR/cwd/d$k"
    wait_for_new "$before" 20 >/dev/null || die "foot d$k did not appear"
    sleep 0.3
  done
  printf 'spike: %s foot windows are on workspace %s. Arrange them into the layout you want to test\n' "$n" "$WS" >&2
  command -v notify-send >/dev/null 2>&1 && notify-send -a spike "spike: arrange $name on workspace $WS" "switch back to workspace $home_ws when done" || true
  printf '       (focus, move, resize with your usual keys; keep them tiled). Switch back to workspace %s when done; waiting for that now\n' "$home_ws" >&2
  while [ "$(active_ws)" = "$WS" ]; do sleep 0.5; done
  sleep 1
  save "shape-$name"
  dump_ws "hand-built shape $name"
  log "  shape $name saved; the next stage moves you off workspace $WS itself"
}

# Computed insertion order for snapshot shape-NAME, prototyped outside hyprsession.
# MODE plain: the pointer is left where it is (what a real load would find).
# MODE steer: the pointer is warped inside the target leaf before every move (needs SPIKE_ALLOW_POINTER=1).
computed() { # name [plain|steer]
  rule_runs
  local name="$1" mode="${2:-plain}" src="shape-$1" rule order_file="$DIR/order-$1-${2:-plain}.txt" dst="computed-$1-${2:-plain}"
  [ -f "$SESS/$src/clients.json" ] || die "no snapshot $src (use shape-insert / shape-hand)"
  rule="${SPIKE_RULE:-$(cat "$DIR/rule.best" 2>/dev/null || echo pointer)}"
  if [ "$mode" = steer ]; then
    [ "$ALLOW_POINTER" = 1 ] || die "steer moves your mouse pointer; set SPIKE_ALLOW_POINTER=1"
    python3 "$MODEL" steer "$SESS/$src/clients.json" "$WS" >"$order_file" || true
  else
    python3 "$MODEL" order "$SESS/$src/clients.json" "$WS" "$rule" --pointer "$(ptr)" >"$order_file" || true
  fi
  log "== computed[$name,$mode] rule=$rule pointer=$(ptr): $(head -1 "$order_file")"
  local lines=() targets=() line
  mapfile -t lines < <(grep -v -e '^MODE' -e '^POINTER' "$order_file")
  mapfile -t targets < <(grep '^POINTER' "$order_file" | sed 's/^POINTER //')
  [ "${#lines[@]}" -gt 0 ] || die "empty order for $name"
  close_ws
  local before="$DIR/before.txt" addrs=() line a i step=0
  for line in "${lines[@]}"; do
    all_addresses >"$before"; dsp_exec "${line#* * }"
    a="$(wait_for_new "$before" 20)" || die "window for '${line#* * }' did not appear"
    addrs+=("$a"); sleep 0.3
  done
  for i in "${!lines[@]}"; do
    if [ "$mode" = steer ] && [ "$i" -ge 1 ]; then
      local t="${targets[$((i - 1))]}"
      warp_pointer "${t% *}" "${t#* }" || log "  !! could not warp the pointer to $t"
    fi
    dsp_move "${addrs[$i]}" "$WS"; sleep "$STEP_PAUSE"
    dump_ws "after inserting #$((i + 1)) (${lines[$i]#* * }) pointer=$(ptr)"
  done
  sleep 1; save "$dst"
  local d; d="$(diff_report "$SESS/$src/clients.json" "$SESS/$dst/clients.json" "$WS")"
  if [ "$d" = EMPTY ]; then log "RESULT computed[$name,$mode]: PASS (first-cycle diff EMPTY)"
  else log "RESULT computed[$name,$mode]: FAIL — diff(saved, reproduced):"; printf '%s\n' "$d" | sed 's/^/     /' | tee -a "$REPORT" >&2; fi
}

# ---------------------------------------------------------------- unfloat variant
# The user's own way of growing a tree: a window is toggled floating, the pointer is put over the
# leaf to split, and the window is unfloated; the unfloat insertion splits the leaf under the pointer.
# Prototype: launch the shape's windows, make them floating, move them onto the scratch workspace
# (floating windows are not tree nodes), then unfloat them in the steer schedule's order, warping the
# pointer into the target leaf's current box before each unfloat.

dsp_float() { # address enable|disable: set the floating state (no-op when already there)
  local cur want=false
  [ "$2" = enable ] && want=true
  cur="$(hyprctl clients -j | jq -r --arg a "$1" '.[] | select(.address == $a) | .floating')"
  [ "$cur" = "$want" ] && return 0
  detect_dialect
  if [ "$DIALECT" = lua ]; then hyprctl dispatch "hl.dsp.window.float({ action = '$2', window = 'address:$1' })" >/dev/null
  else hyprctl dispatch togglefloating "address:$1" >/dev/null; fi
}

table_tsv_tiled() { # like table_tsv, tiled windows only (floating ones are not tree nodes)
  local pid x y w h
  clients_on "$WS" | jq -r '.[] | select(.floating == false) | "\(.pid)\t\(.at[0])\t\(.at[1])\t\(.size[0])\t\(.size[1])"' |
    while IFS=$'\t' read -r pid x y w h; do printf '%s\t%s\t%s\t%s\t%s\n' "$(tag "$pid")" "$x" "$y" "$w" "$h"; done
}

place_pointer() { # x y : warp, or ask the user (with screen fractions) when warping is not accepted
  if [ "$POINTER_MODE" != manual ] && warp_pointer "$1" "$2"; then return 0; fi
  local frac
  frac="$(hyprctl monitors -j | jq -r --argjson x "$1" --argjson y "$2" '.[] | select(.focused) |
      "\(((($x - .x) * 100 / .width) | floor))% across, \(((($y - .y) * 100 / .height) | floor))% down"')"
  printf 'spike: put the mouse pointer at %s,%s (%s of the screen); waiting for it to settle there\n' "$1" "$2" "$frac" >&2
  command -v notify-send >/dev/null 2>&1 && notify-send -a spike "spike: unfloat pointer" "put the pointer at $frac of the screen; the run continues once it settles" || true
  local ptries=0
  while [ "$ptries" -lt 120 ]; do
    awk -v g="$(ptr)" -v w="$1,$2" 'BEGIN{split(g,a,",");split(w,b,",");dx=a[1]-b[1];dy=a[2]-b[2];exit !(dx*dx+dy*dy<=1600)}' && break
    ptries=$((ptries+1)); sleep 0.5
  done
}

unfloat() { # name [hidden|visible]
  rule_runs
  local name="$1" mode="${2:-hidden}" src="shape-$1" tag_="unfloat-$1-${2:-hidden}"
  [ "$ALLOW_POINTER" = 1 ] || die "unfloat moves your mouse pointer; set SPIKE_ALLOW_POINTER=1"
  if [ "$mode" = visible ]; then
    [ "$ALLOW_SWITCH" = 1 ] || die "visible mode switches to workspace $WS and back; set SPIKE_ALLOW_SWITCH=1"
  fi
  [ -f "$SESS/$src/clients.json" ] || die "no snapshot $src"
  local order_file="$DIR/order-$tag_.txt" lines=() targets=() line a i start_ws before="$DIR/before.txt" ptr0
  python3 "$MODEL" steer "$SESS/$src/clients.json" "$WS" >"$order_file" || true
  log "== unfloat[$name,$mode]: $(head -1 "$order_file")"
  mapfile -t lines < <(grep -v -e '^MODE' -e '^POINTER' "$order_file")
  mapfile -t targets < <(grep '^POINTER' "$order_file" | sed 's/^POINTER //')
  [ "${#lines[@]}" -gt 0 ] || die "empty order for $name"
  ptr0="$(ptr)"; start_ws="$(active_ws)"
  close_ws
  local addrs=()
  for line in "${lines[@]}"; do
    all_addresses >"$before"; dsp_exec "${line#* * }"
    a="$(wait_for_new "$before" 20)" || die "window for '${line#* * }' did not appear"
    dsp_float "$a" enable          # floats where it was born, on the visible workspace
    dsp_move "$a" "$WS"            # a floating window joins no tree
    addrs+=("$a"); sleep 0.3
  done
  local floating; floating="$(clients_on "$WS" | jq '[.[] | select(.floating == true)] | length')"
  [ "$floating" -eq "${#lines[@]}" ] || log "  !! only $floating of ${#lines[@]} windows are floating on workspace $WS before the unfloats (run unreliable)"
  [ "$(clients_on "$WS" | jq '[.[] | select(.floating == false)] | length')" -eq 0 ] || log "  !! tiled windows already on workspace $WS before the unfloats (run unreliable)"
  local rd="$RUNS/$tag_" tags=() pointers=() step=0
  rm -rf "$rd"; mkdir -p "$rd"
  if [ "$mode" = visible ]; then
    dsp_workspace "$WS"; sleep 0.5
    log "  VISIBLE MODE: switched the visible workspace from $start_ws to $WS (DEC-B relaxed, conditional on need)"
  fi
  for i in "${!lines[@]}"; do
    step=$((step + 1))
    if [ "$i" -ge 1 ]; then
      local t="${targets[$((i - 1))]}"
      place_pointer "${t% *}" "${t#* }"
      pointers+=("$(ptr)")
    fi
    log "  [$tag_] step $step: unfloat #$step (${lines[$i]#* * })  pointer=$(ptr)"
    dsp_float "${addrs[$i]}" disable
    sleep "$STEP_PAUSE"
    table_tsv_tiled >"$rd/t$step.tsv"
    tags+=("$(tag "$(clients_on "$WS" | jq -r --arg a "${addrs[$i]}" '.[] | select(.address == $a) | .pid')")")
    dump_ws "after unfloating #$step"
  done
  if [ "$mode" = visible ]; then
    dsp_workspace "$start_ws"; sleep 0.5
    log "  VISIBLE MODE: switched back to workspace $start_ws"
  fi
  [ "$ptr0" = "?" ] || place_pointer "${ptr0%,*}" "${ptr0#*,}" || true
  jq -n --arg label "$tag_" --arg order "${tags[*]}" --arg pointers "${pointers[*]:-}" '
    {label: $label, order: ($order | split(" ")), mode: "unfloat",
     pointers: ($pointers | split(" ") | map(select(. != "?") | split(",") | map(tonumber)))}' >"$rd/meta.json"
  python3 "$MODEL" derive "$rd" --out "$RUNS/unfloat.jsonl" | tee -a "$REPORT" >&2
  sleep 1; save "$tag_"
  local d; d="$(diff_report "$SESS/$src/clients.json" "$SESS/$tag_/clients.json" "$WS")"
  if [ "$d" = EMPTY ]; then log "RESULT unfloat[$name,$mode]: PASS (first-cycle diff EMPTY)"
  else log "RESULT unfloat[$name,$mode]: FAIL — diff(saved, reproduced):"; printf '%s\n' "$d" | sed 's/^/     /' | tee -a "$REPORT" >&2; fi
}

# Hidden mode first on s4 (control), s3l and h5; visible mode only for the ones that failed hidden
unfloat_bar() {
  rule_runs
  [ "$ALLOW_POINTER" = 1 ] || die "unfloat moves your mouse pointer; set SPIKE_ALLOW_POINTER=1"
  local name failed=()
  for name in s4 s3l h5; do
    [ -f "$SESS/shape-$name/clients.json" ] || { log "unfloat-bar: no snapshot shape-$name (run bar first), skipped"; continue; }
    unfloat "$name" hidden
    grep -q "^RESULT unfloat\[$name,hidden\]: PASS" "$REPORT" || failed+=("$name")
  done
  if [ "${#failed[@]}" -gt 0 ]; then
    if [ "$ALLOW_SWITCH" = 1 ]; then
      log "-- hidden mode failed for: ${failed[*]}; trying visible mode (switches workspace and back)"
      for name in "${failed[@]}"; do unfloat "$name" visible; done
    else
      log "-- hidden mode failed for: ${failed[*]}; visible mode needs SPIKE_ALLOW_SWITCH=1 (switches to workspace $WS and back)"
    fi
  fi
  log "== unfloat-bar summary"
  grep -E '^RESULT unfloat' "$REPORT" | tee -a "$REPORT.summary" >&2 || true
}

# The pre-stated success bar: first-cycle diff EMPTY for each of these shapes.
bar() {
  rule_runs
  log "== bar: shapes s4 (the spike #1 shape), s3r (3-window, full-height RIGHT), s5, s6 built by insertion;"
  log "   s3l (3-window, full-height LEFT: the ws4/ws7 pattern) and h5 (5 windows) arranged by hand"
  local name n
  for spec in "s4 4" "s3r 3" "s5 5" "s6 6"; do
    name="${spec% *}"; n="${spec#* }"
    shape_insert "$name" "$n"
    park_pointer BL
    computed "$name" plain
    [ "$ALLOW_POINTER" = 1 ] && computed "$name" steer
  done
  log "-- hand-built shapes: you will be asked to arrange windows on workspace $WS (visible), then switch away"
  log "   s3l: one window full-height on the LEFT, two stacked on the RIGHT"
  shape_hand s3l 3
  park_pointer BL; computed s3l plain; [ "$ALLOW_POINTER" = 1 ] && computed s3l steer
  log "   h5: any 5-window layout you actually use"
  shape_hand h5 5
  park_pointer BL; computed h5 plain; [ "$ALLOW_POINTER" = 1 ] && computed h5 steer
  [ "$ALLOW_POINTER" = 1 ] && unfloat_bar
  log "== bar summary"
  grep -E '^RESULT' "$REPORT" | tee -a "$REPORT.summary" >&2 || true
  if [ -n "${START_WS:-}" ]; then dsp_workspace "$START_WS"; log "  returned you to workspace $START_WS"; fi
  command -v notify-send >/dev/null 2>&1 && notify-send -a spike "spike: bar done" "results are in the report" || true
}

spike2() { rule; bar; }

case "${1:-}" in
  preflight) need hyprctl; mkdir -p "$DIR" "$SESS"; : >"$REPORT"; preflight ;;
  build) build ;;
  baseline) baseline ;;
  proto) proto ;;
  proto-focus) proto_focus ;;
  export-fixture) export_fixture "${2:-spike-a}" "${3:?usage: export-fixture SNAPSHOT OUT.json}" ;;
  all) all ;;
  rule) rule ;;
  shape-insert) shape_insert "${2:?name}" "${3:?n}" ;;
  shape-hand) shape_hand "${2:?name}" "${3:?n}" ;;
  computed) computed "${2:?name}" "${3:-plain}" ;;
  bar) bar ;;
  unfloat) unfloat "${2:?name}" "${3:-hidden}" ;;
  unfloat-bar) unfloat_bar ;;
  spike2) spike2 ;;
  cleanup) cleanup ;;
  selftest) selftest ;;
  *) sed -n '2,26p' "$0"; exit 2 ;;
esac
