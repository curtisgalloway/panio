// Copyright 2026 Curtis Galloway
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! hdmicap's connection to the target's hid daemon, for the noVNC view's
//! keyboard and mouse.
//!
//! hdmicap serves RFB to browsers (`rfb_serve.rs`); what a browser types or
//! points is translated there into the HID serial protocol and sent here. The
//! hid daemon is found through its discovery file, named by the env var
//! [`ENV_HID_DISCOVERY`] and re-read on every (re)connect, so a restarted
//! daemon (new port, new token) is picked up. The transport is the daemon's
//! `GET /hid` WebSocket with `Authorization: Bearer`; replies come back as
//! broadcast `evt` frames and are only drained.
//!
//! Everything goes through one bounded queue, in order, so a click lands where
//! the pointer was sent. The writer coalesces what it can without reordering:
//! of several pointer moves in a row it sends only the last, so a client that
//! moves faster than the wire can carry costs nothing but the intermediate
//! positions. Key and button lines are never dropped while connected.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::debug;

/// Env var naming the target's hid daemon discovery file (`daemon.json`).
pub const ENV_HID_DISCOVERY: &str = "HDMICAP_HID_DISCOVERY";

/// Lines waiting for the wire. Generous for typing bursts; a full queue makes
/// the sending session wait rather than drop a key release.
const QUEUE: usize = 256;

/// How long to wait before trying a daemon that just refused us again.
const RETRY_AFTER: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

enum Item {
    Line(String),
    Move(u16, u16),
}

/// Handle to the link task. Cheap to share behind an `Arc`.
pub struct HidLink {
    tx: mpsc::Sender<Item>,
}

impl HidLink {
    /// Start the link task. Must be called inside a tokio runtime.
    pub fn spawn(discovery: PathBuf) -> Arc<HidLink> {
        let (tx, rx) = mpsc::channel(QUEUE);
        tokio::spawn(run(discovery, rx));
        Arc::new(HidLink { tx })
    }

    /// Queue a command line (`down A`, `mdown left`, ...), waiting for room.
    pub async fn send(&self, line: String) {
        let _ = self.tx.send(Item::Line(line)).await;
    }

    /// Queue a command line without waiting; for cleanup that cannot await.
    /// A full queue drops the line.
    pub fn try_send(&self, line: String) {
        let _ = self.tx.try_send(Item::Line(line));
    }

    /// Point at (`x`, `y`) in the 0..=32767 space. Droppable: a full queue
    /// loses it, and the writer skips it when another move follows directly.
    pub fn move_to(&self, x: u16, y: u16) {
        let _ = self.tx.try_send(Item::Move(x, y));
    }

    /// Like [`move_to`](Self::move_to) but waits for room, for the position a
    /// button press must happen at.
    pub async fn move_to_ordered(&self, x: u16, y: u16) {
        let _ = self.tx.send(Item::Move(x, y)).await;
    }
}

use crate::proxy::Ws;

async fn connect(discovery: &Path) -> Result<Ws> {
    crate::proxy::upstream_ws(discovery, "/hid")
        .await
        .context("hid daemon")
}

async fn run(discovery: PathBuf, mut rx: mpsc::Receiver<Item>) {
    let mut ws: Option<Ws> = None;
    let mut failed_at: Option<Instant> = None;
    // The last position written, so an unchanged one is not sent again.
    let mut sent_pos: Option<(u16, u16)> = None;

    loop {
        let mut batch: Vec<Item> = Vec::new();
        tokio::select! {
            item = rx.recv() => match item {
                Some(i) => batch.push(i),
                None => return,
            },
            // Drain (and notice the end of) the daemon's evt broadcast.
            msg = async {
                match ws.as_mut() {
                    Some(w) => w.next().await,
                    None => std::future::pending().await,
                }
            } => {
                match msg {
                    Some(Ok(Message::Text(t))) if t.starts_with("evt err") => {
                        debug!("hid daemon: {t}");
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                        debug!("hid daemon connection closed");
                        ws = None;
                        sent_pos = None;
                    }
                    Some(Ok(_)) => {}
                }
                continue;
            }
        }
        while let Ok(i) = rx.try_recv() {
            batch.push(i);
        }

        if ws.is_none() && failed_at.is_none_or(|t| t.elapsed() >= RETRY_AFTER) {
            match tokio::time::timeout(CONNECT_TIMEOUT, connect(&discovery)).await {
                Ok(Ok(w)) => {
                    ws = Some(w);
                    failed_at = None;
                }
                Ok(Err(e)) => {
                    debug!("hid link: {e:#}");
                    failed_at = Some(Instant::now());
                }
                Err(_) => {
                    debug!("hid link: connect timed out");
                    failed_at = Some(Instant::now());
                }
            }
        }
        let Some(w) = ws.as_mut() else {
            continue; // no daemon: input is dropped
        };

        // In order, but of a run of moves only the last matters.
        let mut ok = true;
        for (n, item) in batch.iter().enumerate() {
            let text = match item {
                Item::Line(l) => l.clone(),
                Item::Move(x, y) => {
                    if matches!(batch.get(n + 1), Some(Item::Move(..)))
                        || sent_pos == Some((*x, *y))
                    {
                        continue;
                    }
                    sent_pos = Some((*x, *y));
                    format!("moveabs {x} {y}")
                }
            };
            if w.send(Message::Text(text)).await.is_err() {
                ok = false;
                break;
            }
        }
        if !ok {
            debug!("hid daemon write failed; will reconnect");
            ws = None;
            sent_pos = None;
        }
    }
}
