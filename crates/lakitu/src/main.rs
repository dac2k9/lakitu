//! lakitu — live TUI for the Claude Code agent activity log.

mod app;
mod client;
mod event;
mod gh;
mod log;
mod remote;
mod store;
mod ui;
mod work;

use std::path::PathBuf;

use clap::Parser;
use color_eyre::Result;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "lakitu",
    version,
    about = "Live TUI for the Claude Code agent activity log",
    long_about = None
)]
struct Cli {
    /// Path to the agent activity log. Defaults to
    /// `$HOME/.claude/logs/agent-actions.log`.
    #[arg(long, short = 'l', env = "AGENT_LOG")]
    log: Option<PathBuf>,

    /// Path to the fleet multi-agent store directory (agents registry +
    /// inboxes). Defaults to `$HOME/.claude/lakitu-fleet`.
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

    /// Watch a remote lakitu daemon (`lakitu-mcp serve`) at this URL instead of
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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
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

    let cli = Cli::parse();

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
            std::env::var("GENBOT_STORE").ok().filter(|s| !s.is_empty()).map(PathBuf::from)
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
            store::Source::Remote(rc) => {
                (rc.snapshot().await.unwrap_or_default(), cli.server.clone().unwrap_or_default())
            }
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

    println!("source {}×{}px, {} bytes png → kitty graphics:", w, h, png.len());
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
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".claude").join("logs").join("agent-actions.log")
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
