#!/bin/sh
# fleet inbox-wake — a Claude Code `Stop` hook.
#
# When an agent goes idle, this checks its fleet inbox; if unread messages
# are waiting, it blocks the stop and tells the agent to read them. The check
# is plain shell, so it costs ZERO model tokens — the model is only
# re-engaged when mail is actually waiting (which you'd pay to read anyway).
#
# Installed as a sibling Stop hook (the herdr-managed hook is left untouched).
# Best-effort throughout: any uncertainty → exit 0 (allow the stop).

# Hook payload arrives as JSON on stdin.
input="$(cat 2>/dev/null || true)"

# Loop guard: if the agent is already continuing because of a Stop hook, let
# it stop now rather than risk nudging forever.
case "$input" in
  *'"stop_hook_active":true'* | *'"stop_hook_active": true'*) exit 0 ;;
esac

# Resolve this agent's fleet name (its inbox/registry key). Names are now
# free-picked, so we can't just assume name == repo short-name:
#   1) $LAKITU_FLEET_NAME if exported — the source of truth. Covers names that
#      differ from the repo, fleet agents whose repo is a glob (code-review),
#      and two-agents-one-repo. Agents export it at session start.
#   2) else the registry entry whose `repo` matches this checkout (so a
#      free-named, one-agent-per-repo client is found automatically).
#   3) else the repo short-name (legacy default; also the file/inbox key).
# Best-effort: if python3 is missing the registry step is skipped → step 3.
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
  name="$(LAKITU_FLEET_SHORT="$short" python3 - <<'PY' 2>/dev/null || true
import os, json, glob
short = os.environ.get("LAKITU_FLEET_SHORT", "")
match = ""
for f in glob.glob(os.path.expanduser("~/.claude/lakitu-fleet/agents/*.json")):
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

# Guard — never gate idle on an inbox this session doesn't own. If the name was
# *guessed* (LAKITU_FLEET_NAME unset), suppress the nudge when the registry shows
# that agent checks out somewhere else: that means we resolved to the cwd-repo's
# owner (a peer), so blocking would loop this session on the peer's inbox.
# Uncertain (python missing, no such entry, or no recorded path) → fall through.
if [ -z "${LAKITU_FLEET_NAME:-${GENBOT_NAME:-}}" ]; then
  here="$(git rev-parse --show-toplevel 2>/dev/null || true)"
  [ -n "$here" ] || here="$PWD"
  foreign="$(LAKITU_NAME_CHK="$name" LAKITU_HERE="$here" python3 - <<'PY' 2>/dev/null || true
import os, json
name = os.environ.get("LAKITU_NAME_CHK", "")
here = os.path.realpath(os.environ.get("LAKITU_HERE", "") or ".")
try:
    d = json.load(open(os.path.expanduser("~/.claude/lakitu-fleet/agents/" + name + ".json")))
    p = d.get("path") or ""
    # Positively foreign only when the registry records a *different* checkout.
    print("foreign" if p and os.path.realpath(p) != here else "")
except Exception:
    print("")
PY
)"
  [ "$foreign" = "foreign" ] && exit 0
fi

# Remote daemon? Ask it for the unread count and gate on that. Fail-soft:
# any error → allow the stop (exit 0).
SERVER="${LAKITU_FLEET_SERVER:-}"
if [ -n "$SERVER" ]; then
  count="$(curl -fsS --max-time 3 -H "Authorization: Bearer ${LAKITU_FLEET_TOKEN:-}" \
    "$SERVER/v1/agents/$name/unread-count" 2>/dev/null \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("unread", 0))
except Exception: print(0)' 2>/dev/null || echo 0)"
  [ "${count:-0}" -gt 0 ] 2>/dev/null || exit 0
  printf '{"decision":"block","reason":"You have %s unread fleet inbox message(s). Call read_inbox(name=\\"%s\\") and triage them per the fleet-coordination skill before going idle."}\n' "$count" "$name"
  exit 0
fi

inbox="$HOME/.claude/lakitu-fleet/inbox/$name"
[ -d "$inbox" ] || exit 0   # not a registered participant → nothing to do

# Count unread messages (top-level *.json; the read/ archive is a subdir).
count="$(find "$inbox" -maxdepth 1 -type f -name '*.json' 2>/dev/null | wc -l | tr -d ' ')"
[ "${count:-0}" -gt 0 ] 2>/dev/null || exit 0

# Mail is waiting — this is the only path that spends tokens. Block the stop
# and point the agent at its inbox.
printf '{"decision":"block","reason":"You have %s unread fleet inbox message(s). Call read_inbox(name=\\"%s\\") and triage them per the fleet-coordination skill before going idle."}\n' "$count" "$name"
exit 0
