#!/usr/bin/env bash
# v1.2 spike: how does dwindle place windows moved onto a workspace, and do ordered moves converge?
#
# Run on the real machine (Hyprland, Lua or classic dispatch dialect) from a checkout of main.
# Everything happens on ONE scratch workspace ($SPIKE_WS, default 9) using foot windows with
# distinct working directories. Nothing else on the desktop is closed or moved: the hyprsession
# loads are scoped (--workspace), and the prototype only touches windows it launched itself.
#
# Commands:  preflight | build | baseline | proto | proto-focus | export-fixture OUT | all | cleanup | selftest
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
    log "$opt: $(hyprctl getoption "$opt" -j 2>/dev/null | jq -c '{int, float, str}' 2>/dev/null || echo unavailable)"
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
  if [ "$ACTIVE_CASE" != 1 ]; then
    [ "$active" != "$WS" ] || die "switch to an empty workspace other than $WS first"
    [ "$(clients_on "$active" | jq 'length')" -eq 0 ] || die "the visible workspace ($active) must be empty"
  fi
}

cleanup() {
  need hyprctl; need jq
  if [ -d "$DIR" ]; then close_ws 2>/dev/null || true; fi
  if [ "${KEEP:-0}" = 1 ]; then log "kept $DIR (it contains your real window titles)"; else rm -rf "$DIR"; echo "removed $DIR" >&2; fi
}

all() {
  build
  baseline
  proto
  log "== done. Paste $REPORT back (it lists no window titles, only fixture directories)."
}

case "${1:-}" in
  preflight) need hyprctl; mkdir -p "$DIR" "$SESS"; : >"$REPORT"; preflight ;;
  build) build ;;
  baseline) baseline ;;
  proto) proto ;;
  proto-focus) proto_focus ;;
  export-fixture) export_fixture "${2:-spike-a}" "${3:?usage: export-fixture SNAPSHOT OUT.json}" ;;
  all) all ;;
  cleanup) cleanup ;;
  selftest) selftest ;;
  *) sed -n '2,20p' "$0"; exit 2 ;;
esac
