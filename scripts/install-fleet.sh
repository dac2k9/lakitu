#!/bin/sh
# Install the Lakitu fleet hooks + coordination skill into ~/.claude and wire
# them into settings.json. Idempotent (safe to re-run) and writes a timestamped
# backup of settings.json before touching it.
#
#   ./scripts/install-fleet.sh
#
# (For a `cargo install`-only setup without the repo, `lakitu-mcp install-hooks`
# does the same from the embedded copies — see the project README.)
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
HOOKS_SRC="$REPO/crates/lakitu/assets/hooks"
SKILL_SRC="$REPO/crates/lakitu/assets/skill/fleet-coordination/SKILL.md"

# Fleet store root: an explicit $LAKITU_FLEET_ROOT / $GENBOT_ROOT wins; otherwise
# the XDG state dir, falling back to the pre-XDG ~/.claude/lakitu-fleet when it
# already exists (don't orphan a running fleet on upgrade).
FLEET="${LAKITU_FLEET_ROOT:-${GENBOT_ROOT:-}}"
if [ -z "$FLEET" ]; then
  XDG="${XDG_STATE_HOME:-$HOME/.local/state}/lakitu/fleet"
  if [ -d "$XDG" ]; then FLEET="$XDG"
  elif [ -d "$HOME/.claude/lakitu-fleet" ]; then FLEET="$HOME/.claude/lakitu-fleet"
  else FLEET="$XDG"; fi
fi
SKILL_DST="$HOME/.claude/skills/fleet-coordination"
SETTINGS="$HOME/.claude/settings.json"

echo "Installing fleet hooks → $FLEET"
mkdir -p "$FLEET" "$SKILL_DST"
for h in state-hook inbox-check context-statusline persona-sessionstart tasks-sessionstart; do
  cp "$HOOKS_SRC/$h.sh" "$FLEET/$h.sh"
  chmod +x "$FLEET/$h.sh"
done
cp "$SKILL_SRC" "$SKILL_DST/SKILL.md"
echo "Installed skill → $SKILL_DST/SKILL.md"

echo "Wiring hooks into $SETTINGS (idempotent; a backup is written first)"
FLEET="$FLEET" SETTINGS="$SETTINGS" python3 - <<'PY'
import json, os, datetime, copy
fleet = os.environ["FLEET"]
path = os.environ["SETTINGS"]
try:
    cfg = json.load(open(path))
except Exception:
    cfg = {}
before = copy.deepcopy(cfg)

def cmd(script, arg=None):
    c = "sh '%s/%s'" % (fleet, script)
    return c + (" " + arg if arg else "")

# (event, script, arg)
WIRING = [
    ("PreToolUse",        "state-hook.sh",          "working"),
    ("PermissionRequest", "state-hook.sh",          "blocked"),
    ("SessionEnd",        "state-hook.sh",          "offline"),
    ("SessionStart",      "state-hook.sh",          "idle"),
    ("SessionStart",      "persona-sessionstart.sh", None),
    ("SessionStart",      "tasks-sessionstart.sh",   None),
    ("Stop",              "inbox-check.sh",          None),
    ("Stop",              "state-hook.sh",           "idle"),
]
hooks = cfg.setdefault("hooks", {})
for event, script, arg in WIRING:
    command = cmd(script, arg)
    blocks = hooks.setdefault(event, [])
    present = any(
        hk.get("command") == command
        for blk in blocks for hk in blk.get("hooks", [])
    )
    if not present:
        blocks.append({"matcher": "*", "hooks": [
            {"type": "command", "command": command, "timeout": 10}]})

# statusLine: context % + rate-limit chip.
sl_cmd = "sh '%s/context-statusline.sh'" % fleet
if (cfg.get("statusLine") or {}).get("command") != sl_cmd:
    cfg["statusLine"] = {"type": "command", "command": sl_cmd, "padding": 0}

if cfg != before:
    if os.path.exists(path):
        bak = path + ".bak-" + datetime.datetime.now().strftime("%Y%m%dT%H%M%S")
        json.dump(before, open(bak, "w"), indent=2)
        print("  backup:", bak)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    json.dump(cfg, open(path, "w"), indent=2)
    open(path, "a").write("\n")
    print("  settings.json updated")
else:
    print("  already wired — no change")
PY

cat <<EOF

Fleet hooks installed. Two more steps to bring an agent online:

  1. Point the agent's MCP at lakitu-mcp. In its .mcp.json (or ~/.claude.json):
       { "mcpServers": { "lakitu-mcp": { "command": "lakitu-mcp" } } }

  2. Export a stable agent name so the hooks find its inbox/presence:
       export LAKITU_FLEET_NAME=<name>

Then run the cockpit:  lakitu
EOF
