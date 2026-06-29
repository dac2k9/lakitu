#!/bin/sh
# Lakitu fleet task loader — a SessionStart hook. Claude Code runs this at the
# start of every session (startup | resume | clear | compact) with a JSON
# payload on stdin. We resolve which fleet agent this checkout is, read its open
# tasks from the store, and return them as `additionalContext` so a reminder the
# agent jotted earlier survives the session boundary (incl. a compaction) and
# resurfaces — the work-equivalent of the persona loader.
#
# Best-effort: no open tasks (or no store / network error) produces no output
# (exit 0), so this never disrupts sessions that haven't opted in. It's a
# SEPARATE hook from persona-sessionstart.sh on purpose — isolated, so a problem
# here can't suppress persona injection (and vice-versa).
#
# Wire it up in ~/.claude/settings.json under hooks.SessionStart:
#   { "type": "command",
#     "command": "sh '$HOME/.local/state/lakitu/fleet/tasks-sessionstart.sh'" }
# (`lakitu install-hooks` writes this path for you.)
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
PINNED="${LAKITU_FLEET_NAME:-${GENBOT_NAME:-}}"
PAYLOAD=$(cat)

LAKITU_ROOT="$ROOT" LAKITU_PINNED="$PINNED" LAKITU_PAYLOAD="$PAYLOAD" \
LAKITU_SERVER="${LAKITU_FLEET_SERVER:-}" LAKITU_TOKEN="${LAKITU_FLEET_TOKEN:-}" python3 -c '
import json, os, sys

try:
    d = json.loads(os.environ.get("LAKITU_PAYLOAD") or "{}")
except Exception:
    d = {}
root = os.environ["LAKITU_ROOT"]
agents = os.path.join(root, "agents")
cwd = d.get("cwd") or os.getcwd()

# Path-safety, mirrors fleet::sanitize in lakitu-mcp so the file we look up
# matches the one add_task wrote.
def sanitize(name):
    out = "".join(c if (c.isalnum() and ord(c) < 128) or c in "._-" else "-" for c in name)
    out = out.strip(".")
    return out or "unnamed"

# Resolve this session to a fleet name (same precedence as the other hooks):
#   1) $LAKITU_FLEET_NAME / $GENBOT_NAME if exported (authoritative),
#   2) else the registry entry whose recorded checkout path == cwd,
#   3) else the cwd basename.
def resolve_name():
    p = os.environ.get("LAKITU_PINNED") or ""
    if p:
        return p
    try:
        rc = os.path.realpath(cwd)
        for f in os.listdir(agents):
            if not f.endswith(".json") or f.endswith(
                (".heartbeat.json", ".wake.json", ".context.json")
            ):
                continue
            try:
                reg = json.load(open(os.path.join(agents, f)))
            except Exception:
                continue
            rp = reg.get("path")
            if rp and os.path.realpath(rp) == rc:
                return reg.get("name") or f[:-5]
    except Exception:
        pass
    return os.path.basename(cwd) if cwd else ""

name = resolve_name()
if not name:
    sys.exit(0)
name = sanitize(name)

# Fetch the raw task list — over HTTP when a daemon is configured, else straight
# from the store file. Either way, a failure → empty list → silent exit.
def load_tasks():
    server = os.environ.get("LAKITU_SERVER") or ""
    if server:
        try:
            import urllib.request
            req = urllib.request.Request(
                server.rstrip("/") + "/v1/agents/" + name + "/tasks",
                headers={"Authorization": "Bearer " + (os.environ.get("LAKITU_TOKEN") or "")})
            resp = urllib.request.urlopen(req, timeout=3)
            if resp.status == 200:
                return (json.load(resp) or {}).get("tasks") or []
        except Exception:
            pass
        return []
    try:
        with open(os.path.join(root, "tasks", name + ".json")) as fh:
            return json.load(fh) or []
    except Exception:
        return []

tasks = [t for t in load_tasks() if isinstance(t, dict) and not t.get("done")]
if not tasks:
    sys.exit(0)  # nothing open → stay silent

def fmt(t):
    line = "- [ ] " + (t.get("text") or "").strip()
    pr = t.get("pr") or {}
    if pr.get("repo") and pr.get("number") is not None:
        line += "  (PR " + str(pr["repo"]) + "#" + str(pr["number"]) + ")"
    if t.get("from_msg"):
        line += "  (from msg " + str(t["from_msg"]) + ")"
    return line

n = len(tasks)
ctx = (
    "# Your open tasks (lakitu fleet — reminders carried across the session boundary)\n\n"
    "You have " + str(n) + " open task" + ("" if n == 1 else "s") + " on your private list, "
    "restored from the fleet store so a reminder you jotted earlier (e.g. a message you "
    "could not action mid-work) survives compaction. Triage them at a safe checkpoint — do not "
    "drop work in hand to service them. Manage with the task MCP tools: `read_tasks` to review, "
    "`complete_task`/`drop_task` by id when done, `add_task` to capture new ones.\n\n"
    + "\n".join(fmt(t) for t in tasks)
)

sys.stdout.write(json.dumps({
    "hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": ctx}
}))
'
