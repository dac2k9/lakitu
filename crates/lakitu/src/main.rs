//! lakitu — one binary, several subcommands.
//!
//! Default (no subcommand) is the live TUI cockpit for the Claude Code agent
//! activity log. The fleet's MCP server + coordination daemon live under verbs:
//!   * `lakitu mcp` — **stdio** MCP, Claude Code's local per-agent transport;
//!   * `lakitu serve` — the **HTTP daemon** (MCP-over-HTTP + a `/v1` REST API);
//!   * `lakitu install-hooks` — materialize the lifecycle hooks + coordination
//!     skill into `~/.claude`.
//!
//! Each subcommand owns its own error type (the TUI uses `color-eyre`, the
//! server/daemon/installer use `anyhow`); the dispatcher converts at the
//! boundary, mapping any error to a stderr print + non-zero exit. tracing is
//! initialized once, per arm — never twice — and the stdio `mcp` arm keeps
//! stdout clean for the JSON-RPC wire by logging to a side-channel file.

use std::io::IsTerminal;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use color_eyre::Result;
use tracing_subscriber::EnvFilter;

use lakitu::{app, client, remote, store};

#[derive(Parser, Debug)]
#[command(
    name = "lakitu",
    version,
    about = "Live TUI cockpit for a fleet of coordinating Claude Code agents",
    long_about = None
)]
struct Cli {
    /// Subcommand. With none, lakitu runs the TUI cockpit (the flags below).
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the agent activity log. Defaults to
    /// `$XDG_STATE_HOME/lakitu/logs/agent-actions.log` (legacy:
    /// `~/.claude/logs/agent-actions.log`).
    #[arg(long, short = 'l', env = "AGENT_LOG")]
    log: Option<PathBuf>,

    /// Path to the fleet multi-agent store directory (agents registry +
    /// inboxes). Defaults to `$XDG_STATE_HOME/lakitu/fleet` (legacy:
    /// `~/.claude/lakitu-fleet`).
    #[arg(long, env = "LAKITU_FLEET_STORE")]
    store: Option<PathBuf>,

    /// Read the fleet store once, print agents + inboxes as text, and
    /// exit — no TUI. Useful for scripting / debugging headless.
    #[arg(long)]
    dump_store: bool,

    /// Your name as a participant ("client") in the network. Joins you to
    /// the roster with your own inbox and lets you send/broadcast from the
    /// cockpit. Remembered across runs; the cockpit prompts on first run if
    /// unset. Also read from `LAKITU_FLEET_ME`.
    #[arg(long, env = "LAKITU_FLEET_ME")]
    me: Option<String>,

    /// Watch a remote lakitu daemon (`lakitu serve`) at this URL instead of
    /// the local store — e.g. `http://host:8787`. Also `$LAKITU_FLEET_SERVER`.
    #[arg(long, env = "LAKITU_FLEET_SERVER")]
    server: Option<String>,

    /// Bearer token for `--server` (must match the daemon's `LAKITU_FLEET_TOKEN`).
    /// Also `$LAKITU_FLEET_TOKEN`.
    #[arg(long, env = "LAKITU_FLEET_TOKEN")]
    token: Option<String>,

    /// Decode an image and print it via the kitty graphics protocol, then
    /// exit — a probe to confirm the terminal renders inline images before we
    /// wire them into the dashboard. Run from the target terminal:
    /// `lakitu --image-test path/to/icon.webp`.
    #[arg(long)]
    image_test: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the stdio MCP server (Claude Code's local per-agent transport).
    Mcp,
    /// Run the HTTP daemon: MCP-over-HTTP plus a `/v1` REST API.
    Serve,
    /// Materialize the fleet lifecycle hooks + coordination skill into `~/.claude`.
    InstallHooks,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    use std::process::ExitCode;

    let cli = Cli::parse();

    match cli.command {
        // The stdio MCP path (formerly the `lakitu-mcp` default mode). tracing
        // goes to a side-channel file because stdout is reserved for the
        // JSON-RPC wire.
        Some(Command::Mcp) => match run_mcp_stdio().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e:#}");
                ExitCode::FAILURE
            }
        },
        // The HTTP daemon (formerly `lakitu-mcp serve`).
        Some(Command::Serve) => match lakitu::daemon::serve().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e:#}");
                ExitCode::FAILURE
            }
        },
        // The installer (formerly `lakitu-mcp install-hooks`).
        Some(Command::InstallHooks) => match lakitu::install::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e:#}");
                ExitCode::FAILURE
            }
        },
        // No subcommand: the TUI cockpit.
        None => {
            // No-tty guard: if no TUI-action flag was passed and stdout isn't a
            // terminal, the user almost certainly meant `lakitu mcp` (e.g. an
            // MCP config still pointing the command at bare `lakitu`). Bail with
            // a hint rather than tearing down a non-existent terminal.
            if cli.image_test.is_none() && !cli.dump_store && !std::io::stdout().is_terminal() {
                eprintln!(
                    "lakitu: no TTY detected — did you mean `lakitu mcp` (run the MCP server)? See `lakitu --help`."
                );
                return ExitCode::FAILURE;
            }
            match run_tui(cli).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("{e:#}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// stdio MCP server (the old `lakitu-mcp` default mode), moved verbatim:
/// tracing → side-channel temp file (stdout stays clean for JSON-RPC), then
/// serve `AgentBoardService` over stdio until the client disconnects.
async fn run_mcp_stdio() -> anyhow::Result<()> {
    use rmcp::{ServiceExt, transport::stdio};

    use lakitu::server::AgentBoardService;

    // tracing goes to a side-channel file because in stdio mode stdout is
    // reserved for the JSON-RPC wire. macOS' temp_dir lands in /var/folders/...;
    // fine for dev. If we ever want persistent logs, switch to the XDG state dir.
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("lakitu-mcp.log"))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(log_file)
        .with_ansi(false)
        .init();

    tracing::info!("lakitu mcp starting (stdio)");
    let service = AgentBoardService::new();
    service.serve(stdio()).await?.waiting().await?;
    Ok(())
}

/// The TUI cockpit (the old `lakitu` default), moved verbatim apart from
/// reading the parsed `Cli` through and reaching modules via the `lakitu::`
/// crate path.
async fn run_tui(cli: Cli) -> Result<()> {
    color_eyre::install()?;

    // Dev logging goes to a file so it doesn't fight the TUI.
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("lakitu.log"))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(log_file)
        .with_ansi(false)
        .init();

    // Probe: render an image inline via the kitty graphics protocol, then exit.
    if let Some(p) = cli.image_test {
        return image_test(&p);
    }

    let log_path = cli.log.unwrap_or_else(default_log_path);
    // Back-compat: the env vars were `GENBOT_*` before the lakitu rename; honor
    // them as a fallback so existing launch configs keep working.
    let store_root = cli
        .store
        .or_else(|| {
            std::env::var("GENBOT_STORE")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(store::default_store_root);

    // Remote daemon (`--server` / $LAKITU_FLEET_SERVER) vs the local store.
    // store_root is still used for local config (your remembered name) either way.
    let source = match cli.server.clone().filter(|s| !s.is_empty()) {
        Some(url) => store::Source::Remote(remote::RemoteClient::new(
            url,
            cli.token.clone().unwrap_or_default(),
        )),
        None => store::Source::Local(store_root.clone()),
    };

    // Resolve the human's "me" name: --me/$LAKITU_FLEET_ME (or legacy $GENBOT_ME)
    // wins (and is remembered); otherwise fall back to a previously-remembered
    // name. None ⇒ the cockpit prompts on first run.
    let me = match cli
        .me
        .or_else(|| std::env::var("GENBOT_ME").ok())
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
    {
        Some(m) => {
            let _ = client::remember_me(&store_root, &m);
            Some(m)
        }
        None => client::load_me(&store_root),
    };
    // Join the network as a client up front, so agents see you (and a
    // --dump-store run reflects it). With no name yet, the cockpit's
    // first-run prompt handles joining instead.
    if let Some(name) = &me {
        match &source {
            store::Source::Local(root) => {
                let _ = client::register_me(root, name);
            }
            store::Source::Remote(rc) => rc.register(name).await,
        }
    }

    if cli.dump_store {
        let (snap, label) = match &source {
            store::Source::Local(r) => (store::read_snapshot(r).await, r.display().to_string()),
            store::Source::Remote(rc) => (
                rc.snapshot().await.unwrap_or_default(),
                cli.server.clone().unwrap_or_default(),
            ),
        };
        dump_store(&snap, &label);
        return Ok(());
    }

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        log = %log_path.display(),
        store = %store_root.display(),
        "starting"
    );

    app::run(log_path, store_root, me, source).await
}

/// Decode an image (webp/png/…) and emit it via the kitty graphics protocol
/// at the cursor, so we can confirm the terminal renders inline images. The
/// image is resized to a small preview; the PNG bytes are base64'd and sent in
/// ≤4096-byte chunks (controls on the first chunk: f=100 PNG, a=T transmit+display).
fn image_test(path: &std::path::Path) -> Result<()> {
    use base64::Engine;
    let img = image::open(path)?;
    let (w, h) = (img.width(), img.height());
    let img = img.resize(160, 160, image::imageops::FilterType::Lanczos3);
    let mut png: Vec<u8> = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);

    println!(
        "source {}×{}px, {} bytes png → kitty graphics:",
        w,
        h,
        png.len()
    );
    let bytes = b64.as_bytes();
    let chunk = 4096;
    let mut i = 0;
    let mut first = true;
    while i < bytes.len() {
        let end = (i + chunk).min(bytes.len());
        let more = u8::from(end < bytes.len());
        let payload = &b64[i..end];
        if first {
            print!("\x1b_Gf=100,a=T,t=d,m={more};{payload}\x1b\\");
            first = false;
        } else {
            print!("\x1b_Gm={more};{payload}\x1b\\");
        }
        i = end;
    }
    println!();
    println!("(if you see the image above, the kitty protocol works here)");
    Ok(())
}

fn default_log_path() -> PathBuf {
    lakitu::paths::agent_actions_log()
}

/// One-shot text dump of the fleet store. Mirrors what the agents pane +
/// inbox view render, for headless inspection.
fn dump_store(snap: &store::StoreSnapshot, label: &str) {
    println!("store: {label}");
    println!("agents ({}):", snap.agents.len());
    for a in &snap.agents {
        let seen = a
            .last_seen
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "never".into());
        println!(
            "  {name:<16} state={state:<8} stale={stale:<5} unread={unread} repo={repo} board={board} last_seen={seen}",
            name = a.name,
            state = a.state.label(),
            stale = a.stale,
            unread = a.unread,
            repo = a.repo,
            board = a.board,
        );
        if let Some(d) = &a.description {
            println!("                   helps: {d}");
        }
        if let Some(t) = &a.task {
            println!("                   task:  {t}");
        }
    }
    let mut names: Vec<&String> = snap.inboxes.keys().collect();
    names.sort();
    for name in names {
        let msgs = &snap.inboxes[name];
        println!("inbox {name} ({} message(s)):", msgs.len());
        for m in msgs {
            let when = m.time.map(|t| t.to_rfc3339()).unwrap_or_default();
            let flag = if m.read { "read  " } else { "UNREAD" };
            println!("  [{flag}] {when}  from {} — {}", m.from, m.title);
            println!("           {}", m.body);
        }
    }
}
