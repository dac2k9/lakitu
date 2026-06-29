#!/bin/sh
# fleet auto-presence — derives an agent's working/idle state from Claude
# Code lifecycle hooks and writes it to the fleet heartbeat, so presence
# (and the cockpit's animated state dot) updates WITHOUT the agent calling
# the heartbeat tool. Costs zero model tokens — it's pure shell + python.
#
#   state-hook.sh working    (PreToolUse: a tool call means active)
#   state-hook.sh idle       (Stop / SessionStart: turn ended / just started)
#   state-hook.sh offline    (SessionEnd: removes the heartbeat → reads offline)
#
# Only touches agents that have REGISTERED (have a registry file), so Claude
# Code sessions in unrelated repos are left completely alone.

state="${1:-}"
case "$state" in
  working | idle | blocked | offline) ;;
  *) exit 0 ;;
esac
# Drain the hook payload on stdin; we don't need it.
cat >/dev/null 2>&1 || true

# Resolve the fleet store root: an explicit $LAKITU_FLEET_ROOT / $GENBOT_ROOT
# wins; otherwise the XDG state dir, falling back to the pre-XDG
# ~/.claude/lakitu-fleet when it already exists (don't orphan a running fleet).
ROOT="${LAKITU_FLEET_ROOT:-${GENBOT_ROOT:-}}"
if [ -z "$ROOT" ]; then
  _xdg="${XDG_STATE_HOME:-$HOME/.local/state}/lakitu/fleet"
  if [ -d "$_xdg" ]; then ROOT="$_xdg"
  elif [ -d "$HOME/.claude/lakitu-fleet" ]; then ROOT="$HOME/.claude/lakitu-fleet"
  else ROOT="$_xdg"; fi
fi

# Resolve the agent name (its registry/heartbeat key). Names are free-picked,
# so we can't just assume name == repo short-name:
#   1) $LAKITU_FLEET_NAME if exported — the source of truth (free names, glob-repo
#      fleet agents like code-review, two-agents-one-repo).
#   2) else the registry entry whose `repo` matches this checkout.
#   3) else the repo short-name (legacy default).
name="${LAKITU_FLEET_NAME:-${GENBOT_NAME:-}}"
if [ -z "$name" ]; then
  remote="$(git remote get-url origin 2>/dev/null || true)"
  if [ -n "$remote" ]; then
    short="$(basename "$remote" .git 2>/dev/null || true)"
  else
    top="$(git rev-parse --show-toplevel 2>/dev/null || true)"
    [ -n "$top" ] || top="$PWD"
    short="$(basename "$top" 2>/dev/null || true)"
  fi
  name="$(LAKITU_FLEET_SHORT="$short" LAKITU_FLEET_AGENTS="$ROOT/agents" python3 - <<'PY' 2>/dev/null || true
import os, json, glob
short = os.environ.get("LAKITU_FLEET_SHORT", "")
match = ""
for f in glob.glob(os.path.join(os.environ.get("LAKITU_FLEET_AGENTS", ""), "*.json")):
    if f.endswith(".heartbeat.json"):
        continue
    try:
        d = json.load(open(f))
    except Exception:
        continue
    if (d.get("repo") or "").rsplit("/", 1)[-1] == short:
        match = d.get("name") or ""
        break
print(match)
PY
)"
  [ -n "$name" ] || name="$short"
fi
[ -n "$name" ] || exit 0

# Remote daemon? Send presence over HTTP (the server replicates the sticky
# blocked logic) and stop — there's no local store to touch. Fail-soft.
SERVER="${LAKITU_FLEET_SERVER:-}"
if [ -n "$SERVER" ]; then
  AUTH="Authorization: Bearer ${LAKITU_FLEET_TOKEN:-}"
  if [ "$state" = "offline" ]; then
    curl -fsS --max-time 3 -X DELETE -H "$AUTH" "$SERVER/v1/agents/$name/state" >/dev/null 2>&1 || true
  else
    curl -fsS --max-time 3 -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
      -d "{\"state\":\"$state\"}" "$SERVER/v1/agents/$name/state" >/dev/null 2>&1 || true
  fi
  exit 0
fi

dir="$ROOT/agents"
[ -f "$dir/$name.json" ] || exit 0   # not a registered agent → ignore
hb="$dir/$name.heartbeat.json"

if [ "$state" = "offline" ]; then
  rm -f "$hb"
  exit 0
fi

command -v python3 >/dev/null 2>&1 || exit 0
LAKITU_FLEET_HB="$hb" LAKITU_FLEET_STATE="$state" python3 - <<'PY'
import datetime, json, os
hb = os.environ["LAKITU_FLEET_HB"]
state = os.environ["LAKITU_FLEET_STATE"]
cur = {}
try:
    with open(hb) as f:
        cur = json.load(f)
except Exception:
    cur = {}
# Two kinds of "blocked": a DELIBERATE one set via the heartbeat MCP tool (no
# marker) is sticky — auto working/idle must not clobber it; the agent clears
# it. An AUTO one (this script, from the Notification / permission hook) just
# means "waiting for you", so any activity (working/idle) clears it.
cur_state = cur.get("state")
cur_auto = bool(cur.get("auto_blocked"))
auto = False
if state == "blocked":
    # Don't downgrade a deliberate blocked into an auto one.
    auto = not (cur_state == "blocked" and not cur_auto)
elif state in ("working", "idle"):
    if cur_state == "blocked" and not cur_auto:
        state = "blocked"  # deliberate blocked stays until the agent clears it
obj = {
    "ts": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
    "state": state,
}
if auto:
    obj["auto_blocked"] = True
task = cur.get("task")
if task:
    obj["task"] = task  # preserve the LM-authored task across auto-updates
try:
    with open(hb, "w") as f:
        json.dump(obj, f)
except Exception:
    pass
PY
