//! Tail the agent activity log.
//!
//! Polls the file every `POLL_INTERVAL` and emits new lines as
//! `Event`s on the returned channel. We use polling instead of
//! `notify` because macOS' FSEvents backend coalesces / drops append
//! events on the same file in practice — newer log lines wouldn't
//! show up live. Polling at 250ms is imperceptibly slow for a
//! human-rate log and works the same on every platform.
//!
//! Malformed lines are silently skipped (logged via `tracing` for
//! the dev's benefit) so a single bad row doesn't kill the TUI.

use std::path::PathBuf;
use std::time::Duration;

use color_eyre::Result;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader, SeekFrom};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};

use crate::event::Event;

const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Spawn the log reader. Yields `Event`s on the returned channel. The
/// channel closes when the producer task exits (UI dropped its
/// receiver). Errors are logged via `tracing` but never bubble up to
/// the TUI — we'd rather show a stale feed than crash.
pub fn spawn(path: PathBuf) -> mpsc::Receiver<Event> {
    let (tx, rx) = mpsc::channel::<Event>(256);
    tokio::spawn(async move {
        if let Err(err) = run(path, tx).await {
            tracing::error!(?err, "log tailer terminated");
        }
    });
    rx
}

async fn run(path: PathBuf, tx: mpsc::Sender<Event>) -> Result<()> {
    let mut last_pos: u64 = 0;
    let mut ticker = interval(POLL_INTERVAL);
    // First tick fires immediately so the existing file is drained on
    // startup; subsequent ticks honor the interval. Skip missed ticks
    // (otherwise a slow tick would burst-fire later).
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        match poll_once(&path, last_pos, &tx).await {
            Ok(new_pos) => last_pos = new_pos,
            Err(err) => {
                tracing::debug!(?err, "log poll error; will retry next tick");
            }
        }
        if tx.is_closed() {
            return Ok(());
        }
    }
}

/// Read everything in the log past byte offset `from`. Returns the new
/// EOF position. If the file shrank (truncate or rotate), starts over
/// from byte 0.
async fn poll_once(path: &PathBuf, from: u64, tx: &mpsc::Sender<Event>) -> Result<u64> {
    let mut file = match File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(from),
        Err(e) => return Err(e.into()),
    };
    let len = file.metadata().await?.len();
    let mut start = from;
    if len < start {
        // File truncated/rotated. Re-read from the beginning.
        start = 0;
        file = File::open(path).await?;
    }
    if len == start {
        return Ok(start); // No new data.
    }
    file.seek(SeekFrom::Start(start)).await?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut pos = start;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        pos += n as u64;
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        if let Some(ev) = Event::from_log_line(trimmed) {
            if tx.send(ev).await.is_err() {
                return Ok(pos);
            }
        } else if !trimmed.is_empty() {
            tracing::debug!(line = %trimmed, "skipping malformed log line");
        }
    }
    Ok(pos)
}
