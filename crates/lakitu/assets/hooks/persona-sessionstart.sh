#!/bin/sh
# Lakitu fleet persona loader — a SessionStart hook. Claude Code runs this at
# the start of every session (source = startup | resume | clear | compact) with
# a JSON payload on stdin. We resolve which fleet agent this checkout is, read
# its self-authored persona (identity + private peer-notes) from the store, and
# return it as `additionalContext` so the agent resumes *being* itself — even
# right after a compaction wiped the in-context version.
#
# Best-effort: a fresh agent with no persona yet produces no output (exit 0),
# so this never disrupts sessions that haven't opted in.
#
# Wire it up in ~/.claude/settings.json under hooks.SessionStart:
#   { "type": "command",
#     "command": "sh '$HOME/.local/state/lakitu/fleet/persona-sessionstart.sh'" }
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
import json, os, sys, glob

try:
    d = json.loads(os.environ.get("LAKITU_PAYLOAD") or "{}")
except Exception:
    d = {}
root = os.environ["LAKITU_ROOT"]
agents = os.path.join(root, "agents")
cwd = d.get("cwd") or os.getcwd()

# Path-safety, mirrors fleet::sanitize in lakitu-mcp so the dir we look up
# matches the one set_identity wrote.
def sanitize(name):
    out = "".join(c if (c.isalnum() and ord(c) < 128) or c in "._-" else "-" for c in name)
    out = out.strip(".")
    return out or "unnamed"

# Resolve this session to a fleet name:
#   1) $LAKITU_FLEET_NAME / $GENBOT_NAME if exported (authoritative),
#   2) else the registry entry whose recorded checkout path == cwd,
#   3) else the cwd basename.
# When >1 registered agent shares this checkout and nothing is pinned, the
# checkout cannot identify the session — handled by the ambiguity branch below.
def path_matches():
    out = []
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
                out.append(reg.get("name") or f[:-5])
    except Exception:
        pass
    return out

pinned = os.environ.get("LAKITU_PINNED") or ""
if pinned:
    name = pinned
else:
    # Dedup by sanitized name so a duplicate registry entry for one agent
    # cannot trip false ambiguity.
    matches = sorted(set(sanitize(m) for m in path_matches()))
    if len(matches) > 1:
        # Shared checkout, nothing pinned → we cannot tell which agent this is.
        # Surface it instead of silently resuming as the wrong one.
        names = ", ".join(matches)
        msg = (
            "# Lakitu fleet: ambiguous identity (no persona loaded)\n"
            "This checkout is shared by multiple registered agents (" + names + "), "
            "and $LAKITU_FLEET_NAME is not set — so this hook cannot tell which one "
            "this session is. To avoid resuming as the wrong agent, no persona was "
            "loaded. Fix: set LAKITU_FLEET_NAME=<your name> for this session "
            "(e.g. via the launcher or .claude/settings) and restart."
        )
        sys.stdout.write(json.dumps({"hookSpecificOutput": {
            "hookEventName": "SessionStart", "additionalContext": msg}}))
        sys.exit(0)
    name = matches[0] if matches else (os.path.basename(cwd) if cwd else "")
if not name:
    sys.exit(0)
name = sanitize(name)

server = os.environ.get("LAKITU_SERVER") or ""
if server:
    # Remote daemon: fetch the rendered persona context over HTTP. Fail-soft —
    # any error / no persona (204) → stay silent so the session is undisturbed.
    try:
        import urllib.request
        req = urllib.request.Request(
            server.rstrip("/") + "/v1/agents/" + name + "/persona",
            headers={"Authorization": "Bearer " + (os.environ.get("LAKITU_TOKEN") or "")})
        resp = urllib.request.urlopen(req, timeout=3)
        if resp.status == 200:
            ctx = (json.load(resp) or {}).get("context") or ""
            if ctx:
                sys.stdout.write(json.dumps({"hookSpecificOutput": {
                    "hookEventName": "SessionStart", "additionalContext": ctx}}))
    except Exception:
        pass
    sys.exit(0)

pdir = os.path.join(root, "personas", name)

identity = ""
try:
    with open(os.path.join(pdir, "identity.md")) as fh:
        identity = fh.read().strip()
except Exception:
    pass

peer_blocks = []
try:
    for pf in sorted(glob.glob(os.path.join(pdir, "peers", "*.md"))):
        peer = os.path.basename(pf)[:-3]
        try:
            with open(pf) as fh:
                body = fh.read().strip()
        except Exception:
            continue
        if body:
            peer_blocks.append("### " + peer + "\n" + body)
except Exception:
    pass

# No persona on file yet — stay silent so fresh agents are undisturbed.
if not identity and not peer_blocks:
    sys.exit(0)

parts = [
    "# Your lakitu persona (persisted identity — resume being this)",
    "You are **" + name + "**. Below is your own self-authored identity and your "
    "private notes on teammates, restored from the fleet store so you carry across "
    "sessions and survive compaction. Speak and act as this character. Keep it "
    "current with the persona MCP tools: `set_identity` when who-you-are shifts, "
    "`remember_peer` when you form or update an impression of a teammate.",
]
if identity:
    parts.append("## Identity\n" + identity)
if peer_blocks:
    parts.append("## Your peers (private notes)\n" + "\n\n".join(peer_blocks))

out = {
    "hookSpecificOutput": {
        "hookEventName": "SessionStart",
        "additionalContext": "\n\n".join(parts),
    }
}
sys.stdout.write(json.dumps(out))
'
