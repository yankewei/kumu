#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../../.." && pwd)"
DEFAULT_BIN="$REPO_ROOT/target/debug/kumu"

die() {
  echo "kumu-verify: $*" >&2
  exit 1
}

cmd_launch() {
  local bin="${KUMU_VERIFY_BIN:-$DEFAULT_BIN}"
  [[ -x "$bin" ]] || die "binary not found or not executable at $bin (run 'cargo build' first)"

  local run_id
  run_id="$(date +%Y%m%d-%H%M%S)-$$"
  local tmp_base="${TMPDIR:-/tmp}"
  tmp_base="${tmp_base%/}"
  # macOS Unix domain sockets have a 104-byte sun_path limit; fall back to /tmp
  # when TMPDIR (e.g. /var/folders/...) would make control.sock exceed it.
  if (( ${#tmp_base} + ${#run_id} + 35 >= 100 )); then
    tmp_base="/tmp"
  fi
  local root="$tmp_base/kumu-verify-$run_id"
  local home_dir="$root/home"
  local project_dir="$root/project"
  local socket_path="$home_dir/.kumu/control.sock"
  local evidence_dir="$tmp_base/kumu-verify-evidence-$run_id"

  mkdir -p "$home_dir/.kumu" "$project_dir" "$evidence_dir"
  chmod 700 "$home_dir/.kumu"

  # Seed a minimal git repository for artifact and pane tests.
  git -C "$project_dir" init -q
  git -C "$project_dir" config user.email "verify@kumu.local"
  git -C "$project_dir" config user.name "Kumu Verify"
  printf '# Verify Repo\n' > "$project_dir/README.md"
  git -C "$project_dir" add README.md
  git -C "$project_dir" commit -q -m "init"

  # Launch Kumu completely isolated from the user's real ~/.kumu and any outer Kumu pane env.
  env -u KUMU_APP_PID -u KUMU_PANE -u KUMU_TERMINAL -u KUMU_TTY -u KUMU_SESSION_TOKEN \
    HOME="$home_dir" \
    KUMU_CONTROL_SOCKET="$socket_path" \
    "$bin" >"$root/kumu.stdout.log" 2>"$root/kumu.stderr.log" &
  local pid=$!
  echo "$pid" > "$root/kumu.pid"

  # Wait until the control socket responds with at least one initialized pane.
  local ready=0
  for _ in $(seq 1 60); do
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "kumu-verify: Kumu exited prematurely (pid $pid). stderr:" >&2
      cat "$root/kumu.stderr.log" >&2 || true
      rm -rf "$root"
      exit 1
    fi
    if [[ -S "$socket_path" ]]; then
      if out="$(env -u KUMU_APP_PID -u KUMU_PANE HOME="$home_dir" KUMU_CONTROL_SOCKET="$socket_path" "$bin" panes --json 2>/dev/null)"; then
        if printf '%s' "$out" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if isinstance(d, list) and len(d) >= 1 else 1)' 2>/dev/null; then
          ready=1
          break
        fi
      fi
    fi
    sleep 0.25
  done

  if [[ "$ready" -ne 1 ]]; then
    echo "kumu-verify: timed out waiting for Kumu control socket at $socket_path" >&2
    kill "$pid" 2>/dev/null || true
    cat "$root/kumu.stderr.log" >&2 || true
    rm -rf "$root"
    exit 1
  fi

  cat <<EOF
export KUMU_VERIFY_RUN_ID='$run_id'
export KUMU_VERIFY_ROOT='$root'
export KUMU_VERIFY_HOME='$home_dir'
export KUMU_VERIFY_SOCKET='$socket_path'
export KUMU_VERIFY_PROJECT='$project_dir'
export KUMU_VERIFY_EVIDENCE='$evidence_dir'
export KUMU_VERIFY_PID='$pid'
export KUMU_VERIFY_BIN='$bin'
EOF
}

cmd_doctor() {
  [[ -n "${KUMU_VERIFY_ROOT:-}" ]] || die "KUMU_VERIFY_ROOT is unset; run 'eval \"\$(... launch)\"' first"
  [[ -n "${KUMU_VERIFY_SOCKET:-}" ]] || die "KUMU_VERIFY_SOCKET is unset"
  [[ -n "${KUMU_VERIFY_HOME:-}" ]] || die "KUMU_VERIFY_HOME is unset"
  [[ -n "${KUMU_VERIFY_PID:-}" ]] || die "KUMU_VERIFY_PID is unset"
  local bin="${KUMU_VERIFY_BIN:-$DEFAULT_BIN}"

  # Guard against driving the user's real instance.
  local real_home
  real_home="$(dscl . -read "/Users/$(id -un)" NFSHomeDirectory 2>/dev/null | awk '{print $2}' || true)"
  if [[ -z "$real_home" ]]; then
    real_home="$(eval echo "~$(id -un)" 2>/dev/null || true)"
  fi
  if [[ -n "$real_home" && "$KUMU_VERIFY_SOCKET" == "$real_home/.kumu/control.sock" ]]; then
    die "refusing to drive real user socket $KUMU_VERIFY_SOCKET"
  fi

  kill -0 "$KUMU_VERIFY_PID" 2>/dev/null || die "Kumu process $KUMU_VERIFY_PID is not running"
  local proc_cmd
  proc_cmd="$(ps -p "$KUMU_VERIFY_PID" -o command= 2>/dev/null || true)"
  [[ "$proc_cmd" == *"$bin"* ]] || die "pid $KUMU_VERIFY_PID ($proc_cmd) does not match $bin"
  [[ -S "$KUMU_VERIFY_SOCKET" ]] || die "control socket missing at $KUMU_VERIFY_SOCKET"

  local panes_json
  panes_json="$(env -u KUMU_APP_PID -u KUMU_PANE HOME="$KUMU_VERIFY_HOME" KUMU_CONTROL_SOCKET="$KUMU_VERIFY_SOCKET" "$bin" panes --json)" \
    || die "kumu panes --json failed"

  local pane_count
  pane_count="$(printf '%s' "$panes_json" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"

  local db_file="$KUMU_VERIFY_HOME/.kumu/kumu.db"
  [[ -f "$db_file" ]] || die "SQLite database missing at $db_file"
  local schema_ver
  schema_ver="$(sqlite3 -readonly "$db_file" "PRAGMA user_version;")"
  [[ "$schema_ver" -gt 0 ]] || die "unexpected SQLite user_version=$schema_ver"

  echo "doctor ok: pid=$KUMU_VERIFY_PID panes=$pane_count schema_v=$schema_ver socket=$KUMU_VERIFY_SOCKET evidence=$KUMU_VERIFY_EVIDENCE"
}

cmd_cli() {
  [[ -n "${KUMU_VERIFY_HOME:-}" && -n "${KUMU_VERIFY_SOCKET:-}" ]] \
    || die "KUMU_VERIFY_HOME / KUMU_VERIFY_SOCKET unset; run launch first"
  local bin="${KUMU_VERIFY_BIN:-$DEFAULT_BIN}"
  env -u KUMU_APP_PID -u KUMU_PANE HOME="$KUMU_VERIFY_HOME" KUMU_CONTROL_SOCKET="$KUMU_VERIFY_SOCKET" "$bin" "$@"
}

cmd_cleanup() {
  local root="${KUMU_VERIFY_ROOT:-}"
  [[ -n "$root" ]] || die "KUMU_VERIFY_ROOT is unset; nothing to clean up"
  [[ "$root" == */kumu-verify-* ]] || die "refusing to delete unexpected path: $root"

  if [[ -f "$root/kumu.pid" ]]; then
    local pid
    pid="$(cat "$root/kumu.pid")"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill -TERM "$pid" 2>/dev/null || true
      for _ in $(seq 1 20); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.25
      done
      if kill -0 "$pid" 2>/dev/null; then
        kill -KILL "$pid" 2>/dev/null || true
      fi
    fi
  fi

  rm -rf "$root"
  echo "cleanup ok: removed $root (evidence preserved at ${KUMU_VERIFY_EVIDENCE:-unset})"
}

case "${1:-}" in
  launch) shift; cmd_launch "$@" ;;
  doctor) shift; cmd_doctor "$@" ;;
  cli) shift; cmd_cli "$@" ;;
  cleanup) shift; cmd_cleanup "$@" ;;
  *)
    echo "usage: $0 <launch|doctor|cli ...|cleanup>" >&2
    exit 2
    ;;
esac
