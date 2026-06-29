# Running the fleet across machines

By default the fleet is a shared local directory (`$XDG_STATE_HOME/lakitu/fleet/`,
i.e. `~/.local/state/lakitu/fleet/`; legacy installs keep using
`~/.claude/lakitu-fleet/`) and
everything — the `lakitu-mcp` MCP, the cockpit, the hooks — reads/writes it
directly. To pool clients across machines, run **one daemon** on a host that
owns that store and have remote machines talk to it over HTTP.

This is **additive**: with none of the env vars below set, every piece keeps
using the local store exactly as before. Remote mode is opt-in per machine.

```
        client machine(s)                         host
  ┌────────────────────────┐            ┌───────────────────────────┐
  │ Claude Code agent       │  MCP/HTTP │  lakitu-mcp serve          │
  │  .mcp.json → http /mcp  │──────────▶│   /mcp   (agent tools)     │
  │ hooks → /v1 (curl)      │           │   /v1/*  (hooks + cockpit) │──▶ ~/.local/state/lakitu/fleet
  └────────────────────────┘           └───────────────────────────┘
  ┌────────────────────────┐                        ▲
  │ cockpit  lakitu --server│────────────────────────┘  GET /v1/snapshot + writes
  └────────────────────────┘
```

All requests carry `Authorization: Bearer $LAKITU_FLEET_TOKEN`.

## 1. Host: run the daemon

```sh
# Build the release binary
cd ~/src/lakitu-mcp && cargo build --release

# A shared secret every client must present
openssl rand -hex 32          # paste into the plist / your secrets store

# Keepalive via launchd (edit the token + listen addr first — see step 2)
cp deploy/com.lakitu.daemon.plist ~/Library/LaunchAgents/
launchctl load -w ~/Library/LaunchAgents/com.lakitu.daemon.plist
launchctl list | grep lakitu  # PID present, last-exit 0
```

Or run it by hand to try it:

```sh
LAKITU_FLEET_TOKEN=secret LAKITU_FLEET_LISTEN=127.0.0.1:8787 \
  ./target/release/lakitu-mcp serve
```

`serve` reuses the same 23 MCP tools as stdio mode and serves the local
`$XDG_STATE_HOME/lakitu/fleet/` store. Stdio mode (no subcommand) is unchanged, so
local agents on the host keep working without the daemon.

## 2. Make it reachable (pick one)

The daemon speaks plain HTTP; put network identity in front of it.

- **tailscale (recommended).** Put both machines on your tailnet, then bind the
  host's tailscale IP so the port is only reachable on the tailnet:
  `LAKITU_FLEET_LISTEN=100.x.y.z:8787` (your `tailscale ip -4`). No certs, no
  public exposure. Clients use `http://100.x.y.z:8787`.
- **Reverse proxy (TLS).** Front it with caddy/nginx for real `https://` if you
  must cross untrusted networks; keep the daemon on `127.0.0.1:8787`.
- **LAN (least safe).** `LAKITU_FLEET_LISTEN=0.0.0.0:8787` exposes it to the
  whole local network, gated only by the bearer token. Fine for a trusted LAN.

Edit `LAKITU_FLEET_LISTEN` in the plist for whichever you choose, then
`launchctl unload … && launchctl load -w …` to apply.

## 3. Connect a remote agent (client machine)

Needs `curl` + `python3` (for the hooks). Set per shell/session, or in your
profile:

```sh
export LAKITU_FLEET_SERVER=http://<host>:8787   # or http://100.x.y.z:8787
export LAKITU_FLEET_TOKEN=secret
export LAKITU_FLEET_NAME=<your-agent-name>       # REQUIRED on a remote machine
```

Point the MCP at the daemon (per-project `.mcp.json` or `~/.claude.json`):

```json
{
  "mcpServers": {
    "lakitu-mcp": {
      "type": "http",
      "url": "http://<host>:8787/mcp",
      "headers": { "Authorization": "Bearer secret" }
    }
  }
}
```

Copy the hook scripts + their wiring so presence / inbox-wake / usage / persona
work remotely (they detect `$LAKITU_FLEET_SERVER` and curl the daemon, falling
back to the local store when it's unset):

```sh
DST="${XDG_STATE_HOME:-$HOME/.local/state}/lakitu/fleet"
mkdir -p "$DST"
scp <host>:"$DST"/'{state-hook,inbox-check,context-statusline,persona-sessionstart}.sh' "$DST"/
# then add the same hooks{} + statusLine blocks to this machine's ~/.claude/settings.json
```

The agent then `register_agent`s (over MCP/HTTP) and appears live in the host's
fleet — presence, inbox, usage, persona all flowing through the daemon.

## 4. Connect a remote cockpit (e.g. laptop)

```sh
lakitu --server http://<host>:8787 --token secret
# or: export LAKITU_FLEET_SERVER / LAKITU_FLEET_TOKEN, then just `lakitu`
```

It polls `GET /v1/snapshot` and routes its writes (messages, projects,
disconnect, register) over HTTP. Without `--server` it reads the local store as
before. (Avatar images aren't fetched over HTTP yet — cosmetic only.)

## 5. Verify

```sh
# Reachability + auth (from the client machine)
curl -fsS -H "Authorization: Bearer secret" http://<host>:8787/v1/snapshot | head -c 200
curl -s -o /dev/null -w '%{http_code}\n' http://<host>:8787/v1/snapshot   # 401 (no token)

# Headless read of the remote fleet
lakitu --server http://<host>:8787 --token secret --dump-store
```

## Notes / failure modes

- **Daemon down** → hooks fail-soft (never block the agent; they `|| true` and
  `--max-time 3`), MCP calls error (the agent still does its core work — fleet
  coordination is "optional plumbing"), and the cockpit keeps showing the last
  snapshot rather than flashing empty.
- **Trust model (v1):** a single shared token; any holder can act as any agent
  (the same trust as "any local process can write any file" today). Per-agent
  tokens are future work — don't put the daemon on the public internet without a
  TLS proxy + network ACLs.
- **Names:** remote agents must `export LAKITU_FLEET_NAME` (the hooks can't infer
  it from a local registry that isn't there).
- **Logs:** launchd → `~/.local/state/lakitu/fleet/daemon.{out,err}.log`; live tracing
  → `$TMPDIR/lakitu-mcp.log`.
