#!/bin/sh
# Lakitu fleet statusline. Claude Code calls this on each status refresh with a
# JSON payload on stdin. We extract this session's context-window usage and
# rate-limit usage, write them into the fleet store keyed by the agent's fleet
# name (so the cockpit can render them), and print a compact line for this
# session's own terminal. Best-effort: never fails the status line.
#
# Wire it up in ~/.claude/settings.json:
#   "statusLine": { "type": "command",
#     "command": "sh '$HOME/.local/state/lakitu/fleet/context-statusline.sh'" }
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
import json, os, sys, time, tempfile
try:
    d = json.loads(os.environ.get("LAKITU_PAYLOAD") or "{}")
except Exception:
    d = {}
agents = os.path.join(os.environ["LAKITU_ROOT"], "agents")
ws = d.get("workspace") or {}

# Resolve the fleet name the same way the cockpit keys clients:
#   1) $LAKITU_FLEET_NAME / $GENBOT_NAME if exported (authoritative),
#   2) else the registry entry whose repo matches this checkout,
#   3) else the repo / cwd basename.
def resolve_name():
    p = os.environ.get("LAKITU_PINNED") or ""
    if p:
        return p
    repo = ws.get("repo") or {}
    owner, rname = repo.get("owner"), repo.get("name")
    if owner and rname:
        target = owner + "/" + rname
        try:
            for f in os.listdir(agents):
                if not f.endswith(".json") or f.endswith(
                    (".heartbeat.json", ".wake.json", ".context.json")
                ):
                    continue
                try:
                    reg = json.load(open(os.path.join(agents, f)))
                except Exception:
                    continue
                if reg.get("repo") == target:
                    return reg.get("name") or f[:-5]
        except Exception:
            pass
    return (repo.get("name") if isinstance(repo, dict) else None) \
        or os.path.basename(ws.get("current_dir") or d.get("cwd") or "") or "unknown"

name = resolve_name()
cw = d.get("context_window") or {}
rl = d.get("rate_limits") or {}
pct = cw.get("used_percentage")
rl5 = (rl.get("five_hour") or {}).get("used_percentage")
rl7 = (rl.get("seven_day") or {}).get("used_percentage")
rl5r = (rl.get("five_hour") or {}).get("resets_at")
rl7r = (rl.get("seven_day") or {}).get("resets_at")

obj = {"pct": pct, "rl5h": rl5, "rl7d": rl7, "rl5h_reset": rl5r, "rl7d_reset": rl7r}
server = os.environ.get("LAKITU_SERVER") or ""
if server:
    # Remote daemon: PUT the usage (ts is stamped server-side). Fail-soft.
    try:
        import urllib.request
        req = urllib.request.Request(
            server.rstrip("/") + "/v1/agents/" + name + "/context",
            data=json.dumps(obj).encode(), method="PUT",
            headers={"Content-Type": "application/json",
                     "Authorization": "Bearer " + (os.environ.get("LAKITU_TOKEN") or "")})
        urllib.request.urlopen(req, timeout=3).read()
    except Exception:
        pass
else:
    try:
        os.makedirs(agents, exist_ok=True)
        obj["ts"] = int(time.time())
        fd, tmp = tempfile.mkstemp(dir=agents)
        with os.fdopen(fd, "w") as f:
            json.dump(obj, f)
        os.replace(tmp, os.path.join(agents, name + ".context.json"))
    except Exception:
        pass

model = (d.get("model") or {}).get("display_name") or ""
cwd = os.path.basename(ws.get("current_dir") or d.get("cwd") or "")
ctx = "ctx " + str(int(pct)) + "%" if pct is not None else ""
sys.stdout.write("  ".join(x for x in (model, cwd, ctx) if x))
'
