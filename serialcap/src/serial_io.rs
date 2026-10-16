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

//! Owns the serial port. A supervisor task opens the port, reconnects on
//! loss/hot-unplug, fans received bytes out to all WebSocket clients via a
//! `broadcast` channel, and drains a single `mpsc` of client input back to the
//! port. A small ring buffer keeps recent output so a client that connects
//! mid-stream sees scrollback (e.g. boot log already in progress).
//!
//! Every received chunk is also tee'd to a dedicated OS thread that owns the
//! [`capture::LineLog`], which assembles timestamped lines and persists them to
//! disk. Keeping that work (and its file I/O) off the supervisor's select loop
//! means a slow disk can never stall the live WebSocket fan-out.

use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc as stdmpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serialport::SerialPort;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_serial::SerialPortBuilderExt;
use tracing::{info, warn};

use crate::capture::LineLog;

const RING_BYTES: usize = 64 * 1024;
const BROADCAST_CAP: usize = 1024;
const WRITE_CAP: usize = 256;
const REOPEN_DELAY: Duration = Duration::from_millis(750);
/// Bytes handed to the port per pass of the supervisor loop (see [`Outbox`]).
const WRITE_CHUNK: usize = 256;
/// Bytes queued in the supervisor before it stops draining `write_tx`, so a
/// flood of input backs up to the HTTP/WebSocket senders (which wait on the
/// channel) instead of growing the queue without bound.
const OUTBOX_HIGH_WATER: usize = 256 * 1024;
/// After the first "open failed" warning, repeat it at most this often.
const OPEN_FAIL_LOG_EVERY: Duration = Duration::from_secs(60);
/// Symlink hops followed when deciding whether a device path is a pty.
const MAX_LINK_HOPS: usize = 16;

#[derive(Clone)]
pub struct Status {
    pub device: String,
    pub baud: u32,
    pub connected: bool,
    generation: u64,
    /// Current power state as read from the configured modem-control sense line.
    /// `None` when no sense signal is configured for this interface.
    pub power_on: Option<bool>,
    pub health: Health,
}

/// Receive identity and progress, independent of synthetic stream markers.
#[derive(Clone, serde::Serialize)]
pub struct Health {
    pub epoch: String,
    pub started_at_ms: u64,
    pub connection_generation: u64,
    pub active_device: Option<String>,
    pub active_baud: Option<u32>,
    pub identity_source: Option<String>,
    pub connected_at_ms: Option<u64>,
    pub last_rx_seq: u64,
    pub last_rx_at_ms: Option<u64>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub capture_through_seq: u64,
    pub capture_through_at_ms: Option<u64>,
    pub capture_alive: bool,
    /// First raw-journal write fault; never cleared by a later successful write.
    pub evidence_error: Option<String>,
    /// Last chunk successfully appended to the raw receive journal (not fsync).
    pub evidence_through_seq: u64,
}

impl Default for Health {
    fn default() -> Self {
        let started_at_ms = crate::capture::now_ms();
        Self {
            epoch: format!(
                "{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            started_at_ms,
            connection_generation: 0,
            active_device: None,
            active_baud: None,
            identity_source: None,
            connected_at_ms: None,
            last_rx_seq: 0,
            last_rx_at_ms: None,
            rx_bytes: 0,
            tx_bytes: 0,
            capture_through_seq: 0,
            capture_through_at_ms: None,
            capture_alive: true,
            evidence_error: None,
            evidence_through_seq: 0,
        }
    }
}

struct CaptureChunk {
    bytes: Bytes,
    received: Option<crate::evidence::ReceiveRecord>,
}

/// One serial interface the daemon owns: a stable `name` (used by the CLI, the
/// dashboard selector, and the capture sub-directory) bound to a `device`/`baud`.
#[derive(Clone)]
pub struct InterfaceSpec {
    pub name: String,
    pub device: String,
    pub baud: u32,
    /// Optional modem-control input pin wired to the target's 3.3 V rail so the
    /// host can detect whether the target is powered on.  Values: "cts", "dsr",
    /// "dcd", "ri".  None when not wired up.
    pub power_sense_signal: Option<String>,
}

/// One running interface: its name paired with the live handle.
#[derive(Clone)]
pub struct NamedSerial {
    pub name: String,
    pub handle: SerialHandle,
}

/// All interfaces the daemon is serving, in the order they were configured. The
/// first is the default for endpoints/commands that don't name one. Cheap to
/// clone (everything inside is channels or `Arc`s).
#[derive(Clone)]
pub struct Serials {
    inner: Arc<Vec<NamedSerial>>,
}

impl Serials {
    /// Spawn a supervisor + capture thread per interface and collect the handles.
    /// Each interface captures into `<capture_base>/<name>/`.
    pub fn spawn_all(specs: &[InterfaceSpec], capture_base: &Path, buffer_lines: u64) -> Self {
        let inner = specs
            .iter()
            .map(|spec| {
                let dir = crate::capture::interface_dir(capture_base, &spec.name);
                let handle = spawn_interface(spec.clone(), dir, buffer_lines);
                NamedSerial {
                    name: spec.name.clone(),
                    handle,
                }
            })
            .collect();
        Serials {
            inner: Arc::new(inner),
        }
    }

    /// Look up an interface by name.
    pub fn get(&self, name: &str) -> Option<&SerialHandle> {
        self.inner
            .iter()
            .find(|ns| ns.name == name)
            .map(|ns| &ns.handle)
    }

    /// The default interface (the first configured), if any.
    pub fn default(&self) -> Option<&NamedSerial> {
        self.inner.first()
    }

    /// All interfaces, in configuration order.
    pub fn all(&self) -> &[NamedSerial] {
        &self.inner
    }
}

/// Handle shared with the HTTP layer. Cheap to clone (all fields are channels
/// or `Arc`s).
#[derive(Clone)]
pub struct SerialHandle {
    /// Serial bytes flowing out to every connected client.
    to_clients: broadcast::Sender<Bytes>,
    /// Client keystrokes flowing back to the port.
    write_tx: mpsc::Sender<WriteRequest>,
    ring: Arc<Mutex<VecDeque<u8>>>,
    status: Arc<Mutex<Status>>,
    /// Button-press requests: caller sends (duration_ms, responder); the
    /// supervisor asserts DTR for that many milliseconds then replies.
    dtr_tx: mpsc::Sender<(u64, oneshot::Sender<Result<(), String>>)>,
    /// This interface's capture directory, where the raw receive journal a
    /// transaction reads its response from lives.
    capture_dir: Arc<PathBuf>,
    /// The per-interface transaction lock (see [`SerialHandle::transaction`]).
    /// A `/transact` holds it exclusively for its whole send-and-wait; plain
    /// input (`/input`, `/stream` keystrokes) takes it shared with `try_*`,
    /// so input is refused rather than queued while a transaction holds the
    /// interface or waits for it, and concurrent plain inputs still coexist.
    txn: Arc<tokio::sync::RwLock<()>>,
    /// Test-only: the capture thread processes each chunk under a shared
    /// hold of this lock, so a test can stall the raw receive journal behind
    /// the live receive cursor (see [`SerialHandle::hold_journal`]).
    #[cfg(test)]
    #[cfg_attr(not(unix), allow(dead_code))]
    journal_gate: Arc<std::sync::RwLock<()>>,
}

impl SerialHandle {
    /// Test-only: stall the capture thread (and so the raw receive journal)
    /// until the returned guard is dropped. The supervisor keeps reading and
    /// advancing `last_rx_seq`, which reproduces the window in which the live
    /// cursor is ahead of the persisted journal.
    ///
    /// The lock is held on a thread of its own, so the guard is a plain value
    /// a test can keep across `.await` points.
    #[cfg(test)]
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn hold_journal(&self) -> JournalHold {
        let gate = self.journal_gate.clone();
        let (held_tx, held_rx) = stdmpsc::channel();
        let (release, released) = stdmpsc::channel::<()>();
        std::thread::spawn(move || {
            let _held = gate.write().unwrap();
            held_tx.send(()).unwrap();
            let _ = released.recv();
        });
        held_rx.recv().unwrap();
        JournalHold { _release: release }
    }

    /// The scrollback snapshot and a live subscription for a new client, taken
    /// together under the ring lock so the two tile exactly: every chunk the
    /// supervisor publishes lands either in the snapshot or on the
    /// subscription — never both, never neither. (`publish` appends to the
    /// ring and broadcasts under the same lock.) Taking them separately left a
    /// window in which a chunk was delivered twice, or not at all.
    pub fn attach(&self) -> (Vec<u8>, broadcast::Receiver<Bytes>) {
        let ring = self.ring.lock().unwrap();
        let rx = self.to_clients.subscribe();
        (ring.iter().copied().collect(), rx)
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    /// Assert DTR for `duration_ms` milliseconds then release.
    ///
    /// Models pressing the J2 power button on a Raspberry Pi (or equivalent).
    /// The caller decides what the press means for the target hardware:
    /// - short press (≤500 ms): OS receives power-button event → graceful reboot/halt
    /// - long press (≥3 s): PMIC hard power-off
    ///
    /// Blocks until the press completes. Concurrent calls queue and execute serially.
    pub async fn dtr_press(&self, duration_ms: u64) -> anyhow::Result<()> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.dtr_tx
            .send((duration_ms, resp_tx))
            .await
            .map_err(|_| anyhow::anyhow!("supervisor not running"))?;
        resp_rx
            .await
            .map_err(|_| anyhow::anyhow!("supervisor dropped response"))?
            .map_err(anyhow::Error::msg)
    }

    /// The interface's capture directory (its raw receive journal).
    pub fn capture_dir(&self) -> &Path {
        &self.capture_dir
    }

    /// Wait for the interface's transaction lock, exclusively. Transactions
    /// queue behind one another in arrival order (tokio's lock is fair); the
    /// caller bounds the wait with its own deadline.
    pub async fn transaction(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        self.txn.clone().write_owned().await
    }

    /// Permission for one plain (non-transaction) write, or `None` while a
    /// transaction holds the interface or is queued for it. Hold the guard
    /// across the write so a transaction cannot start in the middle of it.
    /// `None` is a refusal, not a reason to wait: input queued behind a
    /// transaction would land inside the *next* transaction's response window.
    pub fn try_input(&self) -> Option<tokio::sync::OwnedRwLockReadGuard<()>> {
        self.txn.clone().try_read_owned().ok()
    }

    /// Wait until every byte has been accepted by the serial driver. Pacing
    /// is enforced by the supervisor after actual writes, not queue insertion.
    /// Requests belong to one connection and are never replayed on reconnect.
    pub async fn write_paced(&self, data: Bytes, pace: Duration) -> anyhow::Result<()> {
        let status = self.status();
        if !status.connected {
            return Err(anyhow::anyhow!("serial interface disconnected"));
        }
        if data.is_empty() {
            return Ok(());
        }
        let (reply, done) = oneshot::channel();
        self.write_tx
            .send(WriteRequest {
                data,
                pace,
                generation: status.generation,
                reply: Some(reply),
            })
            .await
            .map_err(|_| anyhow::anyhow!("supervisor not running"))?;
        done.await
            .map_err(|_| anyhow::anyhow!("serial write interrupted"))?
            .map_err(anyhow::Error::msg)
    }
}

/// Test-only: keeps the capture thread stalled until dropped (see
/// [`SerialHandle::hold_journal`]).
#[cfg(test)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct JournalHold {
    _release: stdmpsc::Sender<()>,
}

/// Spawn the supervisor for one interface on the current tokio runtime and return
/// its handle. `capture_dir` / `buffer_lines` configure that interface's on-disk
/// line log; its capture thread is started here and fed via a non-blocking channel.
pub fn spawn_interface(
    spec: InterfaceSpec,
    capture_dir: PathBuf,
    buffer_lines: u64,
) -> SerialHandle {
    let (to_clients, _) = broadcast::channel(BROADCAST_CAP);
    let (write_tx, write_rx) = mpsc::channel(WRITE_CAP);
    let (dtr_tx, dtr_rx) = mpsc::channel::<(u64, oneshot::Sender<Result<(), String>>)>(1);
    let ring = Arc::new(Mutex::new(VecDeque::with_capacity(RING_BYTES)));
    let status = Arc::new(Mutex::new(Status {
        device: spec.device.clone(),
        baud: spec.baud,
        connected: false,
        generation: 0,
        power_on: None,
        health: Health::default(),
    }));

    #[cfg(test)]
    let journal_gate = Arc::new(std::sync::RwLock::new(()));
    let line_tx = spawn_capture(
        capture_dir.clone(),
        buffer_lines,
        status.clone(),
        #[cfg(test)]
        journal_gate.clone(),
    );

    tokio::spawn(supervisor(
        spec,
        to_clients.clone(),
        write_rx,
        dtr_rx,
        ring.clone(),
        status.clone(),
        line_tx,
    ));

    SerialHandle {
        to_clients,
        write_tx,
        ring,
        status,
        dtr_tx,
        capture_dir: Arc::new(capture_dir),
        txn: Arc::new(tokio::sync::RwLock::new(())),
        #[cfg(test)]
        journal_gate,
    }
}

/// Start the OS thread that owns the line log and return a non-blocking sender
/// for raw byte chunks. An unbounded channel means the supervisor never blocks
/// on capture; serial throughput is tiny, so the queue can't grow meaningfully.
///
/// The log defers rewrites of its pending-line sidecar (see
/// `LineLog::sidecar_due`); when one is outstanding the thread waits with a
/// deadline so an idle port still gets its last partial line mirrored.
fn spawn_capture(
    capture_dir: PathBuf,
    buffer_lines: u64,
    status: Arc<Mutex<Status>>,
    #[cfg(test)] journal_gate: Arc<std::sync::RwLock<()>>,
) -> stdmpsc::Sender<CaptureChunk> {
    let (tx, rx) = stdmpsc::channel::<CaptureChunk>();
    std::thread::Builder::new()
        .name("serialcap-capture".into())
        .spawn(move || {
            use stdmpsc::RecvTimeoutError;
            let mut journal = crate::evidence::Journal::open(capture_dir.clone());
            let mut log = LineLog::open(capture_dir, buffer_lines);
            let epoch = status.lock().unwrap().health.epoch.clone();
            if let Err(e) = journal.start_epoch(epoch) {
                status.lock().unwrap().health.evidence_error = Some(e.to_string());
            }
            loop {
                let next = match log.sidecar_due(Instant::now()) {
                    None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    Some(wait) => rx.recv_timeout(wait),
                };
                match next {
                    Ok(chunk) => {
                        #[cfg(test)]
                        let _gate = journal_gate.read().unwrap();
                        log.ingest(&chunk.bytes);
                        if let Some(mut record) = chunk.received {
                            record.captured_at_ms = crate::capture::now_ms();
                            let persisted = journal.append(&record);
                            let mut st = status.lock().unwrap();
                            if let Err(e) = persisted {
                                warn!("raw serial evidence write failed: {e}");
                                st.health
                                    .evidence_error
                                    .get_or_insert_with(|| e.to_string());
                            } else {
                                st.health.evidence_through_seq = record.rx_seq;
                            }
                            st.health.capture_through_seq = record.rx_seq;
                            st.health.capture_through_at_ms = Some(record.rx_at_ms);
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => log.flush_sidecar(),
                    // Every sender has dropped: the daemon is shutting down.
                    Err(RecvTimeoutError::Disconnected) => {
                        log.flush_sidecar();
                        break;
                    }
                }
            }
        })
        .expect("spawn capture thread");
    tx
}

/// `device` with symlinks followed, so a link to a pty (the hidrig console
/// bridge publishes one) is judged by where it points rather than by its own
/// name. Resolved link by link with `read_link` rather than `canonicalize`,
/// which needs the target to exist — this is decided before the open, and a
/// VM's console may not be there yet. `..` and `.` components are folded
/// lexically so a relative link target still yields a `/dev/...` path.
fn resolve_links(device: &str) -> String {
    let mut path = PathBuf::from(device);
    for _ in 0..MAX_LINK_HOPS {
        let Ok(target) = std::fs::read_link(&path) else {
            break;
        };
        path = if target.is_absolute() {
            target
        } else {
            let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
            base.join(target)
        };
    }
    let mut folded = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                folded.pop();
            }
            Component::CurDir => {}
            other => folded.push(other),
        }
    }
    folded.to_string_lossy().into_owned()
}

/// True when `device` names (or links to) a pseudo-terminal slave rather than
/// a real serial port: `/dev/ttysNNN` on macOS, `/dev/pts/N` on Linux. That
/// is what `utmctl attach` prints for a UTM guest's console and what qemu
/// reports for `-serial pty`, so it is the shape a VM's console arrives in.
///
/// A path test rather than an fd test on purpose — the decision has to be made
/// *before* the open, because on macOS it changes how the open is performed.
fn is_pty(device: &str) -> bool {
    is_pty_path(&resolve_links(device))
}

/// The pure prefix test behind [`is_pty`], on an already-resolved path.
fn is_pty_path(device: &str) -> bool {
    let numbered = |rest: &str| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit());
    device.strip_prefix("/dev/ttys").is_some_and(numbered)
        || device.strip_prefix("/dev/pts/").is_some_and(numbered)
}

/// The baud rate to *open* `device` with, which is not always the configured one.
///
/// macOS applies a serial port's line rate with the `IOSSIOSPEED` ioctl, which
/// pseudo-terminals do not implement: the open fails outright with `ENOTTY`
/// ("Not a typewriter"), so before this paniolo could not attach to a VM
/// console at all. The `serialport` crate skips that ioctl when the requested
/// rate is 0, which is its documented way to open a pty — and a pty has no line
/// rate to set in the first place, so nothing is lost by not setting one.
///
/// Linux needs no such thing (its ptys accept the ordinary termios path) and
/// must not be given it: there a rate of 0 is written into `c_ospeed` as `B0`,
/// which means *hang up the line*.
fn open_baud(device: &str, baud: u32) -> u32 {
    if cfg!(target_os = "macos") && is_pty(device) {
        0
    } else {
        baud
    }
}

/// Linux reports the actual opened handle, even if a stable alias is retargeted.
fn opened_device(port: &tokio_serial::SerialStream) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        std::fs::read_link(format!("/proc/self/fd/{}", port.as_raw_fd()))
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        port.name()
    }
}

/// Read one modem-control sense signal and translate it to `power_on`.
///
/// The target's 3.3 V rail is wired (with a pull-down) to the chosen FTDI input
/// pin.  The pin is HIGH when the rail is up (power on) and LOW when off.  FTDI
/// signal sense is active-low in RS-232 convention, so `read_*()` returns
/// `true` when the pin is LOW — meaning powered off.  We invert to get a
/// natural `power_on = true` when the board is running.
fn read_power_sense(port: &mut impl SerialPort, signal: &str) -> Option<bool> {
    match signal {
        "cts" => port.read_clear_to_send().ok().map(|v| !v),
        "dsr" => port.read_data_set_ready().ok().map(|v| !v),
        "dcd" => port.read_carrier_detect().ok().map(|v| !v),
        "ri" => port.read_ring_indicator().ok().map(|v| !v),
        _ => None,
    }
}

/// One caller's write, bound to the connection it was submitted against.
struct WriteRequest {
    data: Bytes,
    pace: Duration,
    generation: u64,
    reply: Option<oneshot::Sender<Result<(), String>>>,
}

impl WriteRequest {
    fn fail(mut self, message: &str) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Err(message.into()));
        }
    }

    fn cancelled(&self) -> bool {
        self.reply.as_ref().is_some_and(|reply| reply.is_closed())
    }
}

/// A bounded FIFO whose acknowledgements follow successful driver writes.
/// Paced messages expose one byte at a time, with the next deadline measured
/// from the last successful write. Reads and DTR remain live while it drains.
#[derive(Default)]
struct Outbox {
    queue: VecDeque<WriteRequest>,
    cursor: usize,
    pending: usize,
    next_byte: Option<tokio::time::Instant>,
}

impl Outbox {
    fn enqueue(&mut self, mut request: WriteRequest) {
        if request.data.is_empty() {
            if let Some(reply) = request.reply.take() {
                let _ = reply.send(Ok(()));
            }
            return;
        }
        self.pending += request.data.len();
        self.queue.push_back(request);
    }

    #[cfg(test)]
    fn push(&mut self, data: Bytes) {
        self.enqueue(WriteRequest {
            data,
            pace: Duration::ZERO,
            generation: 0,
            reply: None,
        });
    }

    fn is_empty(&self) -> bool {
        self.pending == 0
    }
    fn pending(&self) -> usize {
        self.pending
    }

    fn ready(&self) -> bool {
        !self.is_empty()
            && self
                .next_byte
                .is_none_or(|t| tokio::time::Instant::now() >= t)
    }

    fn wait(&self) -> Duration {
        let poll = Duration::from_millis(50);
        if self.ready() {
            return poll;
        }
        self.next_byte.map_or(poll, |t| {
            t.saturating_duration_since(tokio::time::Instant::now())
                .min(poll)
        })
    }

    fn front(&self) -> &[u8] {
        match self.queue.front() {
            Some(head) => {
                let chunk = if head.pace.is_zero() { WRITE_CHUNK } else { 1 };
                &head.data[self.cursor..(self.cursor + chunk).min(head.data.len())]
            }
            None => &[],
        }
    }

    fn consume(&mut self, n: usize) {
        let Some(head) = self.queue.front() else {
            return;
        };
        let n = n.min(head.data.len() - self.cursor);
        if n == 0 {
            return;
        }
        self.cursor += n;
        self.pending -= n;
        self.next_byte = if head.pace.is_zero() {
            None
        } else {
            Some(tokio::time::Instant::now() + head.pace)
        };
        if self.cursor == head.data.len() {
            let mut done = self.queue.pop_front().unwrap();
            self.cursor = 0;
            if let Some(reply) = done.reply.take() {
                let _ = reply.send(Ok(()));
            }
        }
    }

    fn discard_cancelled(&mut self) {
        while self.queue.front().is_some_and(WriteRequest::cancelled) {
            let request = self.queue.pop_front().unwrap();
            self.pending -= request.data.len() - self.cursor;
            self.cursor = 0;
        }
    }
}

impl Drop for Outbox {
    fn drop(&mut self) {
        for request in self.queue.drain(..) {
            request.fail(
                "serial connection lost before write completed; input may have been partially sent",
            );
        }
    }
}

/// Rate limit for the "open failed" warning. A missing device is retried every
/// [`REOPEN_DELAY`], and warning on every attempt wrote 10-15 MB a day to
/// daemon.log for one unplugged adapter. Log the first failure, then at most
/// one a minute, then once on recovery.
struct OpenFailures {
    attempts: u64,
    last_logged: Option<Instant>,
}

impl OpenFailures {
    fn new() -> Self {
        OpenFailures {
            attempts: 0,
            last_logged: None,
        }
    }

    /// Record a failed attempt; `Some(attempts so far)` when this one should
    /// be logged.
    fn failed(&mut self, now: Instant) -> Option<u64> {
        self.attempts += 1;
        let due = match self.last_logged {
            None => true,
            Some(last) => now.duration_since(last) >= OPEN_FAIL_LOG_EVERY,
        };
        if due {
            self.last_logged = Some(now);
            Some(self.attempts)
        } else {
            None
        }
    }

    /// Record a successful open; `Some(failed attempts)` when failures
    /// preceded it, so the recovery can be logged once.
    fn recovered(&mut self) -> Option<u64> {
        self.last_logged = None;
        let n = std::mem::take(&mut self.attempts);
        (n > 0).then_some(n)
    }
}

/// Why the supervisor's read/write loop stopped.
enum InnerExit {
    Disconnect,
    DtrPress {
        duration_ms: u64,
        resp_tx: oneshot::Sender<Result<(), String>>,
    },
}

async fn supervisor(
    spec: InterfaceSpec,
    to_clients: broadcast::Sender<Bytes>,
    mut write_rx: mpsc::Receiver<WriteRequest>,
    mut dtr_rx: mpsc::Receiver<(u64, oneshot::Sender<Result<(), String>>)>,
    ring: Arc<Mutex<VecDeque<u8>>>,
    status: Arc<Mutex<Status>>,
    line_tx: stdmpsc::Sender<CaptureChunk>,
) {
    let InterfaceSpec {
        device,
        baud,
        power_sense_signal,
        ..
    } = spec;

    // Track whether we've ever connected so the first open shows "connected"
    // and later opens show "reconnected".
    let mut ever_connected = false;
    let mut open_failures = OpenFailures::new();

    // A pty (a VM's console) is both opened and torn down differently from a
    // real port; see `open_baud` and the `Ok(0)` arm of the read loop.
    let pty = is_pty(&device);
    let baud = open_baud(&device, baud);
    if pty {
        info!("{device} is a pty — opening without a line rate");
    }

    loop {
        // DTR may be the target's power button (see `dtr_press`), so the port
        // is opened with it de-asserted rather than left at the OS default:
        // Linux raises DTR on open and drops it on close, which made every
        // open — daemon start, every reconnect — a driver-timed press. Linux
        // still pulses it for the duration of the open itself; that is in the
        // tty core and out of reach from here.
        let port = match tokio_serial::new(&device, baud)
            .dtr_on_open(false)
            .open_native_async()
        {
            Ok(mut p) => {
                match open_failures.recovered() {
                    Some(n) => {
                        info!("serial port opened: {device} @ {baud} (after {n} failed attempts)")
                    }
                    None => info!("serial port opened: {device} @ {baud}"),
                }
                {
                    let mut st = status.lock().unwrap();
                    st.generation = st.generation.wrapping_add(1);
                    st.connected = true;
                    st.health.connection_generation = st.generation;
                    st.health.active_device = opened_device(&p);
                    st.health.identity_source = Some(
                        if cfg!(target_os = "linux") {
                            "opened_fd"
                        } else {
                            "port_name"
                        }
                        .into(),
                    );
                    st.health.active_baud = Some(baud);
                    st.health.connected_at_ms = Some(crate::capture::now_ms());
                    if let Some(sig) = &power_sense_signal {
                        st.power_on = read_power_sense(&mut p, sig);
                    }
                }
                if ever_connected {
                    emit_marker(&ring, &to_clients, &line_tx, "reconnected", 32);
                // green
                } else {
                    emit_marker(&ring, &to_clients, &line_tx, "connected", 36); // cyan
                    ever_connected = true;
                }
                p
            }
            Err(e) => {
                match open_failures.failed(Instant::now()) {
                    Some(1) => warn!(
                        "open {device} failed: {e} (retrying every {REOPEN_DELAY:?}; \
                         further failures are logged once a minute)"
                    ),
                    Some(n) => warn!("open {device} still failing after {n} attempts: {e}"),
                    None => {}
                }
                status.lock().unwrap().connected = false;
                let retry = tokio::time::sleep(REOPEN_DELAY);
                tokio::pin!(retry);
                loop {
                    tokio::select! {
                        _ = &mut retry => break,
                        Some(request) = write_rx.recv() => request.fail("serial interface disconnected"),
                    }
                }
                continue;
            }
        };

        let generation = status.lock().unwrap().generation;
        let (mut rd, mut wr) = tokio::io::split(port);
        let mut buf = [0u8; 65536];
        let mut outbox = Outbox::default();

        // One open port. A DTR press pauses this loop but does not leave it;
        // only a disconnect does.
        loop {
            let exit = loop {
                outbox.discard_cancelled();
                tokio::select! {
                    read = rd.read(&mut buf) => match read {
                        Ok(0) if pty => {
                            // A pty *does* have an EOF: `Ok(0)` means the far end
                            // closed — the VM powered off, or its hypervisor let go
                            // of the master. Treat it as the disconnect it is rather
                            // than spinning, so the marker is emitted and the
                            // reconnect loop can pick the console back up.
                            break InnerExit::Disconnect;
                        }
                        Ok(0) => {
                            // Real serial ports don't have EOF; Ok(0) means the async
                            // read resolved without data. Yield to avoid a spin loop.
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                        Ok(n) => {
                            let at = crate::capture::now_ms();
                            let record = {
                                let mut st = status.lock().unwrap();
                                st.health.last_rx_seq += 1;
                                st.health.last_rx_at_ms = Some(at);
                                st.health.rx_bytes += n as u64;
                                crate::evidence::ReceiveRecord {
                                    epoch: st.health.epoch.clone(),
                                    rx_seq: st.health.last_rx_seq,
                                    connection_generation: st.health.connection_generation,
                                    rx_at_ms: at,
                                    captured_at_ms: 0,
                                    bytes: buf[..n].to_vec(),
                                }
                            };
                            let chunk = Bytes::copy_from_slice(&buf[..n]);
                            if line_tx.send(CaptureChunk { bytes: chunk.clone(), received: Some(record) }).is_err() {
                                status.lock().unwrap().health.capture_alive = false;
                            }
                            publish_live(&ring, &to_clients, chunk);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                        Err(e) => { warn!("serial read error: {e}"); break InnerExit::Disconnect; }
                    },
                    // One bounded slice per pass, so the read arm above keeps
                    // its turn while a large paste drains (see `Outbox`).
                    written = wr.write(outbox.front()), if outbox.ready() => match written {
                        Ok(0) => tokio::time::sleep(Duration::from_millis(1)).await,
                        Ok(n) => {
                            status.lock().unwrap().health.tx_bytes += n as u64;
                            outbox.consume(n);
                        },
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                        Err(e) => { warn!("serial write error: {e}"); break InnerExit::Disconnect; }
                    },
                    // Past the high-water mark the queue is left in the
                    // channel, where senders wait on it.
                    Some(data) = write_rx.recv(), if outbox.pending() < OUTBOX_HIGH_WATER => {
                        if data.generation != generation {
                            data.fail("serial connection changed before write");
                        } else {
                            outbox.enqueue(data);
                        }
                    },
                    // Wake for paced writes and to discard input whose caller
                    // disconnected while a serial write was backpressured.
                    _ = tokio::time::sleep(outbox.wait()), if !outbox.is_empty() => {},
                    Some((duration_ms, resp_tx)) = dtr_rx.recv() => {
                        break InnerExit::DtrPress { duration_ms, resp_tx };
                    }
                }
            };

            match exit {
                InnerExit::DtrPress {
                    duration_ms,
                    resp_tx,
                } => {
                    // Rejoin the split halves to regain the SerialPort trait methods.
                    let mut port = rd.unsplit(wr);
                    emit_marker(&ring, &to_clients, &line_tx, "button press", 35); // magenta
                    let result = pulse_dtr(
                        |level| port.write_data_terminal_ready(level),
                        Duration::from_millis(duration_ms),
                    )
                    .await;
                    // Read power state immediately after releasing the button — the
                    // 3.3 V rail may have dropped (long press → power-off).
                    if let Some(sig) = &power_sense_signal {
                        status.lock().unwrap().power_on = read_power_sense(&mut port, sig);
                    }
                    let failed = result.is_err();
                    resp_tx.send(result).ok();
                    if failed {
                        // A failed release may leave DTR asserted. Close the
                        // failed handle and reconnect with DTR deasserted.
                        {
                            let mut st = status.lock().unwrap();
                            st.connected = false;
                            st.power_on = None;
                        }
                        emit_marker(&ring, &to_clients, &line_tx, "disconnected", 31);
                        drop(port);
                        tokio::time::sleep(REOPEN_DELAY).await;
                        break;
                    }
                    // Keep the port open. Closing and reopening it here let the
                    // OS drop and re-raise DTR on the way back in — a second,
                    // driver-timed button press after every deliberate one.
                    let (r, w) = tokio::io::split(port);
                    rd = r;
                    wr = w;
                }
                InnerExit::Disconnect => {
                    // We only reach here after a successful open, so this is a real
                    // disconnect (link dropped / device unplugged), not a failed open.
                    status.lock().unwrap().connected = false;
                    emit_marker(&ring, &to_clients, &line_tx, "disconnected", 31); // red
                    tokio::time::sleep(REOPEN_DELAY).await;
                    break;
                }
            }
        }
    }
}

/// Always attempt release, including when assertion itself failed. Keep both
/// errors if neither operation succeeded so a failed release is never hidden.
async fn pulse_dtr(
    mut set: impl FnMut(bool) -> serialport::Result<()>,
    duration: Duration,
) -> Result<(), String> {
    let asserted = set(true);
    if asserted.is_ok() {
        tokio::time::sleep(duration).await;
    }
    let released = set(false);
    match (asserted, released) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), Ok(())) => Err(format!("asserting DTR failed: {e}")),
        (Ok(()), Err(e)) => Err(format!("releasing DTR failed: {e}")),
        (Err(a), Err(r)) => Err(format!(
            "asserting DTR failed: {a}; releasing DTR failed: {r}"
        )),
    }
}

/// A styled, timestamped status line for the stream, so the web terminal
/// shows exactly when the serial link dropped or came back. ANSI color `code`
/// (31 red / 32 green / 33 yellow / 36 cyan); only the WS terminal renders
/// it — `tio` uses a different path.
pub(crate) fn marker_line(label: &str, code: u8) -> Bytes {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let sod = secs % 86_400;
    let line = format!(
        "\r\n\x1b[1;{code}m── serial {label} [{:02}:{:02}:{:02} UTC] ──\x1b[0m\r\n",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60,
    );
    Bytes::from(line.into_bytes())
}

/// Inject a [`marker_line`] into the stream and scrollback.
fn emit_marker(
    ring: &Arc<Mutex<VecDeque<u8>>>,
    to_clients: &broadcast::Sender<Bytes>,
    line_tx: &stdmpsc::Sender<CaptureChunk>,
    label: &str,
    code: u8,
) {
    publish(ring, to_clients, line_tx, marker_line(label, code));
}

/// Fan one chunk out: the capture thread, the scrollback ring and every live
/// client. The ring is appended and the broadcast sent under the ring lock,
/// so a client attaching at the same moment (`SerialHandle::attach`, which
/// snapshots and subscribes under that lock) sees the chunk exactly once.
/// The critical section is a ≤64 KiB memcpy, so the lock is taken outright —
/// the old `try_lock` skipped the ring whenever a client was reading it,
/// leaving holes in the scrollback.
fn publish(
    ring: &Arc<Mutex<VecDeque<u8>>>,
    to_clients: &broadcast::Sender<Bytes>,
    line_tx: &stdmpsc::Sender<CaptureChunk>,
    chunk: Bytes,
) {
    if line_tx
        .send(CaptureChunk {
            bytes: chunk.clone(),
            received: None,
        })
        .is_err()
    {
        warn!("capture thread dead — bytes lost");
    }
    publish_live(ring, to_clients, chunk);
}

fn publish_live(
    ring: &Arc<Mutex<VecDeque<u8>>>,
    to_clients: &broadcast::Sender<Bytes>,
    chunk: Bytes,
) {
    let mut r = ring.lock().unwrap();
    push_ring(&mut r, &chunk);
    // Err just means no subscribers; that's fine.
    let _ = to_clients.send(chunk);
}

/// Append `chunk` to the scrollback ring, evicting the oldest bytes past
/// [`RING_BYTES`].
fn push_ring(ring: &mut VecDeque<u8>, chunk: &[u8]) {
    ring.extend(chunk.iter().copied());
    let overflow = ring.len().saturating_sub(RING_BYTES);
    if overflow > 0 {
        ring.drain(0..overflow);
    }
}

/// Enumerate serial ports on this host.
pub fn list_ports() -> anyhow::Result<Vec<(String, String)>> {
    let ports = tokio_serial::available_ports()?;
    Ok(ports
        .into_iter()
        .map(|p| (p.port_name, describe(&p.port_type)))
        .collect())
}

fn describe(t: &tokio_serial::SerialPortType) -> String {
    use tokio_serial::SerialPortType;
    match t {
        SerialPortType::UsbPort(info) => {
            let product = info.product.as_deref().unwrap_or("");
            let manuf = info.manufacturer.as_deref().unwrap_or("");
            format!("USB {:04x}:{:04x} {manuf} {product}", info.vid, info.pid)
                .trim()
                .to_string()
        }
        SerialPortType::PciPort => "PCI".into(),
        SerialPortType::BluetoothPort => "Bluetooth".into(),
        SerialPortType::Unknown => "unknown".into(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio_serial::{SerialPortType, UsbPortInfo};

    fn snapshot(r: &VecDeque<u8>) -> Vec<u8> {
        r.iter().copied().collect()
    }

    // ── push_ring: scrollback ring-buffer truncation ────────────────────────

    #[test]
    fn push_ring_accumulates_in_order() {
        let mut r = VecDeque::new();
        push_ring(&mut r, b"hello ");
        push_ring(&mut r, b"world");
        assert_eq!(snapshot(&r), b"hello world");
    }

    #[test]
    fn push_ring_truncates_to_capacity_keeping_newest() {
        let mut r = VecDeque::new();
        push_ring(&mut r, &vec![b'A'; RING_BYTES]); // fill exactly to capacity
        push_ring(&mut r, b"XYZ"); // 3 bytes over -> 3 oldest dropped
        let snap = snapshot(&r);
        assert_eq!(snap.len(), RING_BYTES, "never grows past RING_BYTES");
        assert_eq!(&snap[snap.len() - 3..], b"XYZ", "newest bytes retained");
        assert!(
            snap[..RING_BYTES - 3].iter().all(|&b| b == b'A'),
            "exactly the 3 oldest bytes were evicted"
        );
    }

    #[test]
    fn push_ring_single_oversized_chunk_keeps_tail() {
        let mut r = VecDeque::new();
        let big: Vec<u8> = (0..(RING_BYTES as u32 + 100)).map(|i| i as u8).collect();
        push_ring(&mut r, &big);
        let snap = snapshot(&r);
        assert_eq!(snap.len(), RING_BYTES);
        assert_eq!(
            snap,
            big[big.len() - RING_BYTES..],
            "keeps the most-recent window"
        );
    }

    // ── publish + attach: a chunk reaches a client exactly once ─────────────

    /// A client attaching while output is flowing must see every chunk once:
    /// in its scrollback snapshot or on its subscription, never both, never
    /// neither. Taking the snapshot and the subscription separately (and
    /// skipping the ring under `try_lock`) left windows for both.
    ///
    /// The producer publishes consecutive numbers as fast as it can while the
    /// test attaches over and over; each attach checks that its snapshot is
    /// gap-free and that the first chunk on its subscription continues the
    /// snapshot's sequence.
    #[test]
    fn attach_tiles_scrollback_and_subscription_exactly() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let ring = Arc::new(Mutex::new(VecDeque::new()));
        let (to_clients, _) = broadcast::channel::<Bytes>(BROADCAST_CAP);
        let (line_tx, line_rx) = stdmpsc::channel::<CaptureChunk>();
        let stop = Arc::new(AtomicBool::new(false));
        let producer = {
            let (ring, to_clients, stop) = (ring.clone(), to_clients.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut n: u32 = 0;
                while !stop.load(Ordering::Relaxed) {
                    publish(
                        &ring,
                        &to_clients,
                        &line_tx,
                        Bytes::copy_from_slice(&n.to_le_bytes()),
                    );
                    n += 1;
                    // Give the sampling thread a turn so it is not perpetually
                    // lagged behind the producer on a loaded machine.
                    std::thread::yield_now();
                }
                n
            })
        };
        // Drain the capture side so its queue does not grow unboundedly.
        let drain = std::thread::spawn(move || for _ in line_rx {});

        let handle = SerialHandle {
            to_clients: to_clients.clone(),
            write_tx: mpsc::channel(1).0,
            ring: ring.clone(),
            status: Arc::new(Mutex::new(Status {
                device: "test".into(),
                baud: 0,
                connected: true,
                generation: 1,
                power_on: None,
                health: Health::default(),
            })),
            dtr_tx: mpsc::channel(1).0,
            capture_dir: Arc::new(PathBuf::new()),
            txn: Default::default(),
            journal_gate: Default::default(),
        };

        // Sample until enough verdicts are in; a lagged sample yields none, and
        // how often that happens depends on machine load, not on correctness.
        let mut checked = 0;
        for _ in 0..50_000 {
            if checked >= 100 {
                break;
            }
            let (snap, mut rx) = handle.attach();
            let mut nums = snap
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_le_bytes(*c));
            let Some(mut last) = nums.next() else {
                continue;
            };
            for n in nums {
                assert_eq!(n, last + 1, "hole in the scrollback: {last} then {n}");
                last = n;
            }
            // The first live chunk must be the one after the snapshot's last.
            let first = loop {
                match rx.try_recv() {
                    Ok(b) => break Some(u32::from_le_bytes(b[..].try_into().unwrap())),
                    Err(broadcast::error::TryRecvError::Empty) => std::thread::yield_now(),
                    // Fell behind the producer between attach and recv: no
                    // verdict from this sample.
                    Err(_) => break None,
                }
            };
            if let Some(first) = first {
                assert_eq!(
                    first,
                    last + 1,
                    "snapshot ended at {last}, subscription began at {first}"
                );
                checked += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        let produced = producer.join().unwrap();
        drop(to_clients);
        drain.join().unwrap();
        assert!(produced > 100, "producer barely ran ({produced} chunks)");
        assert!(
            checked >= 100,
            "too few verdicts to mean anything ({checked})"
        );
    }

    // ── Outbox: bounded writes per loop pass ────────────────────────────────

    #[test]
    fn outbox_hands_out_at_most_one_chunk_per_pass_in_order() {
        let mut ob = Outbox::default();
        let data: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        ob.push(Bytes::from(data.clone()));
        assert_eq!(ob.pending(), 1000);
        let mut written = Vec::new();
        let mut passes = 0;
        while !ob.is_empty() {
            let slice = ob.front();
            assert!(
                slice.len() <= WRITE_CHUNK,
                "a pass wrote {} bytes",
                slice.len()
            );
            assert!(!slice.is_empty());
            written.extend_from_slice(slice);
            let n = slice.len();
            ob.consume(n);
            passes += 1;
        }
        assert_eq!(written, data, "bytes reach the port intact and in order");
        assert_eq!(passes, 1000_usize.div_ceil(WRITE_CHUNK));
        assert!(ob.front().is_empty());
    }

    #[test]
    fn outbox_short_write_resumes_mid_message_and_crosses_messages() {
        let mut ob = Outbox::default();
        ob.push(Bytes::from_static(b"abcdef"));
        ob.push(Bytes::from_static(b"gh"));
        assert_eq!(ob.front(), b"abcdef");
        ob.consume(2); // the port took only two bytes
        assert_eq!(ob.front(), b"cdef");
        assert_eq!(ob.pending(), 6);
        ob.consume(4);
        assert_eq!(ob.front(), b"gh", "moves on to the next message");
        ob.consume(2);
        assert!(ob.is_empty());
        ob.consume(1); // nothing to consume: harmless
        assert!(ob.is_empty());
    }

    #[test]
    fn outbox_ignores_empty_messages() {
        let mut ob = Outbox::default();
        ob.push(Bytes::new());
        assert!(ob.is_empty());
        assert!(ob.front().is_empty());
    }

    // ── OpenFailures: one warning, then one a minute, then the recovery ──────

    #[test]
    fn open_failures_log_first_then_once_a_minute_then_recovery() {
        let t0 = Instant::now();
        let mut f = OpenFailures::new();
        assert_eq!(f.failed(t0), Some(1), "the first failure is logged");
        assert_eq!(f.failed(t0 + Duration::from_millis(750)), None);
        assert_eq!(f.failed(t0 + Duration::from_secs(30)), None);
        assert_eq!(
            f.failed(t0 + OPEN_FAIL_LOG_EVERY),
            Some(4),
            "a minute later it is logged again, with the attempt count"
        );
        assert_eq!(
            f.failed(t0 + OPEN_FAIL_LOG_EVERY + Duration::from_secs(1)),
            None
        );
        assert_eq!(
            f.recovered(),
            Some(5),
            "recovery reports the failed attempts"
        );
        assert_eq!(f.recovered(), None, "a clean open has nothing to report");
        assert_eq!(f.failed(t0), Some(1), "the cycle restarts after recovery");
    }

    #[tokio::test]
    async fn dtr_errors_are_reported_and_release_is_always_attempted() {
        for failures in [[false, false], [true, false], [false, true], [true, true]] {
            let mut calls = Vec::new();
            let result = pulse_dtr(
                |level| {
                    let fail = failures[calls.len()];
                    calls.push(level);
                    if fail {
                        Err(serialport::Error::new(
                            serialport::ErrorKind::Unknown,
                            "injected failure",
                        ))
                    } else {
                        Ok(())
                    }
                },
                Duration::ZERO,
            )
            .await;
            assert_eq!(calls, [true, false]);
            assert_eq!(result.is_err(), failures.iter().any(|f| *f));
            if let Err(message) = result {
                assert_eq!(message.contains("asserting"), failures[0]);
                assert_eq!(message.contains("releasing"), failures[1]);
            }
        }
    }

    #[tokio::test]
    async fn dtr_handle_propagates_the_supervisors_error() {
        let (mut handle, _rx) = test_handle();
        let (tx, mut rx) = mpsc::channel(1);
        handle.dtr_tx = tx;
        let worker = tokio::spawn(async move {
            let (_, reply) = rx.recv().await.unwrap();
            reply.send(Err("releasing DTR failed".into())).unwrap();
        });
        let error = handle.dtr_press(1).await.unwrap_err();
        assert!(error.to_string().contains("releasing DTR failed"));
        worker.await.unwrap();
    }

    // ── describe: port-type formatting ──────────────────────────────────────

    #[test]
    fn describe_usb_with_full_info() {
        let info = UsbPortInfo {
            vid: 0x0403,
            pid: 0x6001,
            serial_number: Some("ABC123".into()),
            manufacturer: Some("FTDI".into()),
            product: Some("FT232R USB UART".into()),
        };
        assert_eq!(
            describe(&SerialPortType::UsbPort(info)),
            "USB 0403:6001 FTDI FT232R USB UART"
        );
    }

    #[test]
    fn describe_usb_trims_when_manufacturer_and_product_absent() {
        let info = UsbPortInfo {
            vid: 0x1234,
            pid: 0x5678,
            serial_number: None,
            manufacturer: None,
            product: None,
        };
        assert_eq!(describe(&SerialPortType::UsbPort(info)), "USB 1234:5678");
    }

    #[test]
    fn describe_non_usb_variants() {
        assert_eq!(describe(&SerialPortType::PciPort), "PCI");
        assert_eq!(describe(&SerialPortType::BluetoothPort), "Bluetooth");
        assert_eq!(describe(&SerialPortType::Unknown), "unknown");
    }

    // ── write_paced: the `serial send` pacing fan-out ───────────────────────

    fn test_handle() -> (SerialHandle, mpsc::Receiver<WriteRequest>) {
        let (to_clients, _) = broadcast::channel(16);
        let (write_tx, write_rx) = mpsc::channel(WRITE_CAP);
        let (dtr_tx, _dtr_rx) = mpsc::channel(1);
        let status = Arc::new(Mutex::new(Status {
            device: "test".into(),
            baud: 115_200,
            connected: false,
            generation: 0,
            power_on: None,
            health: Health::default(),
        }));
        let handle = SerialHandle {
            to_clients,
            write_tx,
            ring: Arc::new(Mutex::new(VecDeque::new())),
            status,
            dtr_tx,
            capture_dir: Arc::new(PathBuf::new()),
            txn: Default::default(),
            journal_gate: Default::default(),
        };
        // _dtr_rx is held by the caller's scope only long enough to build the
        // handle; write_paced never touches the DTR path.
        drop(_dtr_rx);
        (handle, write_rx)
    }

    #[tokio::test]
    async fn disconnected_write_is_rejected_without_queueing() {
        let (handle, mut rx) = test_handle();
        assert!(handle
            .write_paced(Bytes::from_static(b"reboot\n"), Duration::ZERO)
            .await
            .is_err());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn write_waits_for_driver_completion() {
        let (handle, mut rx) = test_handle();
        handle.status.lock().unwrap().connected = true;
        let mut task = tokio::spawn(async move {
            handle
                .write_paced(Bytes::from_static(b"abc"), Duration::ZERO)
                .await
        });
        let request = rx.recv().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut task)
                .await
                .is_err(),
            "queueing must not acknowledge delivery"
        );
        let mut outbox = Outbox::default();
        outbox.enqueue(request);
        outbox.consume(1);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut task)
                .await
                .is_err(),
            "a short write is not completion"
        );
        outbox.consume(2);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn disconnect_fails_outstanding_write() {
        let (handle, mut rx) = test_handle();
        handle.status.lock().unwrap().connected = true;
        let task = tokio::spawn(async move {
            handle
                .write_paced(Bytes::from_static(b"abc"), Duration::ZERO)
                .await
        });
        let mut outbox = Outbox::default();
        outbox.enqueue(rx.recv().await.unwrap());
        outbox.consume(1);
        drop(outbox);
        assert!(task
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("partially sent"));
    }

    #[tokio::test]
    async fn pacing_starts_after_each_successful_write() {
        let mut outbox = Outbox::default();
        outbox.enqueue(WriteRequest {
            data: Bytes::from_static(b"abc"),
            pace: Duration::from_millis(30),
            generation: 0,
            reply: None,
        });
        assert_eq!(outbox.front(), b"a");
        // Simulate a driver stall longer than the requested pace.
        tokio::time::sleep(Duration::from_millis(40)).await;
        outbox.consume(1);
        assert!(!outbox.ready(), "elapsed queue time must not allow a burst");
        assert_eq!(outbox.front(), b"b");
        tokio::time::sleep(Duration::from_millis(35)).await;
        assert!(outbox.ready());
    }

    #[tokio::test]
    async fn cancelled_write_discards_unsent_bytes() {
        let (reply, done) = oneshot::channel();
        let mut outbox = Outbox::default();
        outbox.enqueue(WriteRequest {
            data: Bytes::from_static(b"abc"),
            pace: Duration::ZERO,
            generation: 0,
            reply: Some(reply),
        });
        outbox.consume(1);
        drop(done);
        outbox.discard_cancelled();
        assert!(outbox.is_empty());
    }

    // ── pty consoles (a VM's serial port) ───────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn is_pty_recognizes_platform_pty_paths() {
        assert!(is_pty("/dev/ttys006"), "macOS pty slave");
        assert!(is_pty("/dev/pts/3"), "Linux pty slave");
    }

    #[test]
    fn is_pty_rejects_real_serial_devices() {
        for dev in [
            "/dev/tty.usbserial-AA00BB11", // macOS FTDI
            "/dev/cu.usbmodem14201",       // macOS CDC-ACM
            "/dev/ttyS0",                  // Linux 16550
            "/dev/ttyUSB0",
            "/dev/ttyACM0",
            "/dev/serial/by-id/usb-FTDI_FT232R-if00-port0",
            "/dev/ttys", // prefix with no number
            "/dev/pts/", // ditto
        ] {
            assert!(!is_pty(dev), "{dev} is not a pty");
        }
    }

    #[cfg(unix)]
    pub(crate) fn tmp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("serialcap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A symlink to a pty is a pty: the hidrig console bridge publishes its
    /// console as a link with a stable name, and by that name alone the
    /// prefix test said "real port" — so on macOS the open applied a line
    /// rate and failed with ENOTTY. The link targets here do not exist, which
    /// is the point: the decision is made before the open, from the path.
    #[cfg(unix)]
    #[test]
    fn is_pty_follows_symlinks_to_the_device() {
        use std::os::unix::fs::symlink;
        let dir = tmp_dir("pty-links");

        let to_mac_pty = dir.join("console");
        symlink("/dev/ttys004", &to_mac_pty).unwrap();
        assert!(is_pty(to_mac_pty.to_str().unwrap()), "link to a macOS pty");

        let to_linux_pty = dir.join("guest");
        symlink("/dev/pts/7", &to_linux_pty).unwrap();
        assert!(
            is_pty(to_linux_pty.to_str().unwrap()),
            "link to a Linux pty"
        );

        let chained = dir.join("chained");
        symlink(&to_linux_pty, &chained).unwrap();
        assert!(is_pty(chained.to_str().unwrap()), "link to a link to a pty");

        let relative = dir.join("relative");
        symlink("./guest", &relative).unwrap();
        assert!(is_pty(relative.to_str().unwrap()), "relative link target");

        let to_real = dir.join("uart");
        symlink("/dev/tty.usbserial-0001", &to_real).unwrap();
        assert!(!is_pty(to_real.to_str().unwrap()), "link to a real port");

        let plain = dir.join("ttys004");
        std::fs::write(&plain, b"").unwrap();
        assert!(
            !is_pty(plain.to_str().unwrap()),
            "a plain file named like a pty"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_baud_drops_the_line_rate_for_a_pty_on_macos_only() {
        // Linux must keep the configured rate: there a 0 becomes B0, "hang up".
        let want = if cfg!(target_os = "macos") {
            0
        } else {
            115_200
        };
        assert_eq!(open_baud("/dev/ttys006", 115_200), want);
        // A real port always keeps its rate, on every platform.
        assert_eq!(open_baud("/dev/tty.usbserial-AA00BB11", 115_200), 115_200);
    }

    /// A pty pair, as a hypervisor hands one out: the returned fd is the end
    /// UTM/qemu keeps, and the path is what it prints for you to attach to.
    #[cfg(unix)]
    pub(crate) fn open_pty_master() -> (std::os::unix::io::RawFd, String) {
        use std::ffi::CStr;
        unsafe {
            let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(fd >= 0, "posix_openpt: {}", std::io::Error::last_os_error());
            assert_eq!(libc::grantpt(fd), 0, "grantpt");
            assert_eq!(libc::unlockpt(fd), 0, "unlockpt");
            let name = libc::ptsname(fd);
            assert!(!name.is_null(), "ptsname");
            (fd, CStr::from_ptr(name).to_string_lossy().into_owned())
        }
    }

    /// The regression this whole path exists for: before `open_baud`, this open
    /// failed on macOS with ENOTTY ("Not a typewriter") and paniolo could not
    /// attach to a VM console at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn opens_a_real_pty_and_carries_data_both_ways() {
        use std::os::unix::io::FromRawFd;

        let (master, device) = open_pty_master();
        assert!(is_pty(&device), "{device} should be recognised as a pty");
        let baud = open_baud(&device, 115_200);
        let mut port = tokio_serial::new(&device, baud)
            .open_native_async()
            .unwrap_or_else(|e| panic!("opening pty {device} at baud {baud}: {e}"));
        let mut far = unsafe { std::fs::File::from_raw_fd(master) };

        std::io::Write::write_all(&mut far, b"boot: hello\r\n").unwrap();
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(
            Duration::from_secs(5),
            AsyncReadExt::read(&mut port, &mut buf),
        )
        .await
        .expect("read from pty timed out")
        .expect("read from pty failed");
        assert_eq!(&buf[..n], b"boot: hello\r\n");

        AsyncWriteExt::write_all(&mut port, b"reply\r\n")
            .await
            .unwrap();
        let mut back = [0u8; 64];
        let n = std::io::Read::read(&mut far, &mut back).unwrap();
        assert_eq!(&back[..n], b"reply\r\n");
    }

    /// The same open through a symlink — the shape the hidrig console bridge
    /// publishes. On macOS this only succeeds if the link is recognised as a
    /// pty (baud 0); with a line rate the open fails with ENOTTY.
    #[cfg(unix)]
    #[tokio::test]
    async fn opens_a_pty_through_a_symlink() {
        use std::os::unix::fs::symlink;
        use std::os::unix::io::FromRawFd;

        let (master, device) = open_pty_master();
        let far = unsafe { std::fs::File::from_raw_fd(master) };
        let dir = tmp_dir("pty-symlink-open");
        let link = dir.join("console");
        symlink(&device, &link).unwrap();
        let link = link.to_str().unwrap().to_string();

        let baud = open_baud(&link, 115_200);
        let port = tokio_serial::new(&link, baud).open_native_async();
        std::fs::remove_dir_all(&dir).ok();
        port.unwrap_or_else(|e| panic!("opening pty via symlink {link} at baud {baud}: {e}"));
        drop(far);
    }

    /// The premise behind the supervisor's `Ok(0) if pty` arm: when the far end
    /// goes away the read must *resolve* — never hang, never yield data — so the
    /// loop can emit "disconnected" and start reconnecting. macOS reports this
    /// as EOF, Linux as EIO; both break the loop, a hang would not.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_closed_pty_far_end_resolves_the_read() {
        use std::os::unix::io::FromRawFd;

        let (master, device) = open_pty_master();
        let mut port = tokio_serial::new(&device, open_baud(&device, 115_200))
            .open_native_async()
            .expect("pty open");
        drop(unsafe { std::fs::File::from_raw_fd(master) }); // the VM went away

        let mut buf = [0u8; 32];
        let read = tokio::time::timeout(
            Duration::from_secs(5),
            AsyncReadExt::read(&mut port, &mut buf),
        )
        .await;
        match read.expect("read hung after the pty far end closed") {
            Ok(0) => {}
            Ok(n) => panic!("read {n} bytes from a pty whose far end is closed"),
            Err(e) => assert_ne!(
                e.kind(),
                std::io::ErrorKind::WouldBlock,
                "WouldBlock would spin the supervisor instead of reconnecting"
            ),
        }
    }

    // ── the supervisor on a real pty ────────────────────────────────────────

    /// A supervisor on the slave end of a fresh pty, with the master end (the
    /// "target") returned for the test to drive. The capture thread writes to
    /// a scratch directory.
    #[cfg(unix)]
    pub(crate) fn spawn_on_pty(tag: &str) -> (SerialHandle, std::fs::File, PathBuf) {
        use std::os::unix::io::FromRawFd;
        let (master, device) = open_pty_master();
        let far = unsafe { std::fs::File::from_raw_fd(master) };
        let dir = tmp_dir(tag);
        let handle = spawn_interface(
            InterfaceSpec {
                name: "console".into(),
                device,
                baud: 115_200,
                power_sense_signal: None,
            },
            dir.clone(),
            100,
        );
        (handle, far, dir)
    }

    /// Wait until the client-side stream has carried `needle`, returning
    /// everything received so far.
    #[cfg(unix)]
    pub(crate) async fn recv_until(rx: &mut broadcast::Receiver<Bytes>, needle: &[u8]) -> Vec<u8> {
        let mut got = Vec::new();
        loop {
            let chunk = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "timed out waiting for {:?}; received so far: {:?}",
                        String::from_utf8_lossy(needle),
                        String::from_utf8_lossy(&got)
                    )
                })
                .expect("stream closed");
            got.extend_from_slice(&chunk);
            if got.windows(needle.len()).any(|w| w == needle) {
                return got;
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn receive_journal_preserves_prompt_before_send_and_later_binary_echo() {
        use std::io::{Read, Write};
        let (handle, mut far, dir) = spawn_on_pty("evidence-prompt");
        let (_, mut rx) = handle.attach();
        recv_until(&mut rx, b"serial connected").await;
        far.write_all(b"prompt> ").unwrap();
        recv_until(&mut rx, b"prompt> ").await;
        wait_for_evidence(&handle).await;
        let epoch = handle.status().health.epoch;
        let before = crate::evidence::read(&dir, Some(&epoch), Some(0), 4096).unwrap();
        assert_eq!(
            before.records.len(),
            1,
            "synthetic connected marker is not RX"
        );
        handle
            .write_paced(Bytes::from_static(b"command\r"), Duration::ZERO)
            .await
            .unwrap();
        let mut input = [0; 8];
        far.read_exact(&mut input).unwrap();
        assert_eq!(&input, b"command\r");
        far.write_all(b"command\r\n\xff\x00\x1b[31mdone\n").unwrap();
        recv_until(&mut rx, b"done\n").await;
        wait_for_evidence(&handle).await;
        let after = crate::evidence::read(&dir, Some(&epoch), Some(0), 4096).unwrap();
        assert_eq!(after.records[0], before.records[0]);
        let subsequent: Vec<u8> = after
            .records
            .iter()
            .skip(1)
            .flat_map(|r| r.bytes.iter().copied())
            .collect();
        assert_eq!(subsequent, b"command\r\n\xff\x00\x1b[31mdone\n");
        assert!(after.records.iter().all(|r| r.connection_generation == 1));
        assert_eq!(
            after.latest.unwrap().rx_seq,
            handle.status().health.last_rx_seq
        );
        drop(far);
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    async fn wait_for_evidence(handle: &SerialHandle) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let health = handle.status().health;
                assert!(
                    health.evidence_error.is_none(),
                    "{:?}",
                    health.evidence_error
                );
                if health.evidence_through_seq == health.last_rx_seq {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn health_separates_quiet_rx_capture_and_disconnect() {
        let (handle, mut far, dir) = spawn_on_pty("health");
        let (_, mut rx) = handle.attach();
        recv_until(&mut rx, b"serial connected").await;
        let quiet = handle.status();
        assert!(quiet.connected);
        assert_eq!(quiet.health.last_rx_seq, 0);
        assert_eq!(quiet.health.rx_bytes, 0);
        assert_eq!(quiet.health.connection_generation, 1);
        assert!(quiet.health.active_device.is_some());
        std::io::Write::write_all(&mut far, b"prompt> ").unwrap();
        recv_until(&mut rx, b"prompt> ").await;
        let active = handle.status();
        assert_eq!(active.health.epoch, quiet.health.epoch);
        assert_eq!(active.health.rx_bytes, 8);
        assert!(active.health.last_rx_at_ms.is_some());
        tokio::time::timeout(Duration::from_secs(3), async {
            while handle.status().health.capture_through_seq != active.health.last_rx_seq {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(far);
        recv_until(&mut rx, b"serial disconnected").await;
        assert!(!handle.status().connected);
        assert_eq!(handle.status().health.rx_bytes, 8);
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn health_reconnect_preserves_epoch_but_changes_open_identity() {
        use std::os::unix::{fs::symlink, io::FromRawFd};
        let (first_fd, first_path) = open_pty_master();
        let mut first = unsafe { std::fs::File::from_raw_fd(first_fd) };
        let dir = tmp_dir("health-reconnect");
        let alias = dir.join("console");
        symlink(&first_path, &alias).unwrap();
        let handle = spawn_interface(
            InterfaceSpec {
                name: "console".into(),
                device: alias.to_string_lossy().into_owned(),
                baud: 115200,
                power_sense_signal: None,
            },
            dir.join("capture"),
            100,
        );
        let (_, mut rx) = handle.attach();
        recv_until(&mut rx, b"serial connected").await;
        std::io::Write::write_all(&mut first, b"before").unwrap();
        recv_until(&mut rx, b"before").await;
        wait_for_evidence(&handle).await;
        let before = handle.status();
        let (second_fd, second_path) = open_pty_master();
        let mut second = unsafe { std::fs::File::from_raw_fd(second_fd) };
        std::fs::remove_file(&alias).unwrap();
        symlink(&second_path, &alias).unwrap();
        assert_eq!(
            handle.status().health.active_device,
            before.health.active_device
        );
        drop(first);
        recv_until(&mut rx, b"serial reconnected").await;
        let after = handle.status();
        assert_eq!(after.health.epoch, before.health.epoch);
        assert_eq!(after.health.connection_generation, 2);
        assert_eq!(after.health.last_rx_seq, before.health.last_rx_seq);
        #[cfg(target_os = "linux")]
        assert_eq!(
            after.health.active_device.as_deref(),
            Some(second_path.as_str())
        );
        std::io::Write::write_all(&mut second, b"after").unwrap();
        recv_until(&mut rx, b"after").await;
        wait_for_evidence(&handle).await;
        let records = crate::evidence::read(
            &dir.join("capture"),
            Some(&before.health.epoch),
            Some(0),
            4096,
        )
        .unwrap()
        .records;
        assert_eq!(records.first().unwrap().connection_generation, 1);
        assert_eq!(records.last().unwrap().connection_generation, 2);
        assert_eq!(
            records.first().unwrap().epoch,
            records.last().unwrap().epoch
        );
        drop(second);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Review M18. A large write must not stop the port being read. The
    /// target here never reads, so the pty's buffer fills after a few KiB and
    /// the rest of the 64 KiB paste cannot go out; the supervisor must still
    /// pick up and fan out what the target prints in the meantime. Before the
    /// outbox, `write_all` held the loop until the paste finished, so this
    /// timed out.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stuck_write_does_not_starve_reads() {
        let (handle, mut far, dir) = spawn_on_pty("stuck-write");
        let (_, mut rx) = handle.attach();
        recv_until(&mut rx, b"serial connected").await;

        let writer = handle.clone();
        let writing = tokio::spawn(async move {
            writer
                .write_paced(Bytes::from(vec![b'x'; 64 * 1024]), Duration::ZERO)
                .await
        });
        // Give the supervisor time to start the write and wedge on the full
        // pty buffer before the target speaks.
        tokio::time::sleep(Duration::from_millis(200)).await;

        std::io::Write::write_all(&mut far, b"target says hi\r\n").unwrap();
        recv_until(&mut rx, b"target says hi").await;
        assert!(
            !writing.is_finished(),
            "blocked writes must not report success"
        );
        writing.abort();
        let _ = writing.await;
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review M19. A DTR press must not close and reopen the port: the OS
    /// drops and re-raises DTR across a reopen, which was a second, driver-
    /// timed button press after every deliberate one (and the read stream lost
    /// whatever arrived meanwhile). The tell is the marker: a reopen emits
    /// "reconnected". Data must keep flowing on the same port afterwards.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dtr_press_keeps_the_port_open() {
        let (handle, mut far, dir) = spawn_on_pty("dtr-press");
        let (_, mut rx) = handle.attach();
        recv_until(&mut rx, b"serial connected").await;

        let press = handle.dtr_press(20).await;
        if press.is_err() {
            // PTYs need not implement modem-control ioctls. A failed ioctl
            // must now be reported and the failed handle reopened.
            recv_until(&mut rx, b"reconnected").await;
        }

        std::io::Write::write_all(&mut far, b"after the press\r\n").unwrap();
        let got = recv_until(&mut rx, b"after the press").await;
        let text = String::from_utf8_lossy(&got);
        if press.is_ok() {
            assert!(text.contains("serial button press"), "{text}");
        }
        assert!(
            !text.contains("reconnected") && !text.contains("disconnected"),
            "the press reopened the port: {text}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_rejects_input_from_an_old_connection() {
        let (handle, _far, dir) = spawn_on_pty("old-generation");
        let (_, mut stream) = handle.attach();
        recv_until(&mut stream, b"serial connected").await;
        let (reply, done) = oneshot::channel();
        handle
            .write_tx
            .send(WriteRequest {
                data: Bytes::from_static(b"reboot\n"),
                pace: Duration::ZERO,
                generation: handle.status().generation.wrapping_sub(1),
                reply: Some(reply),
            })
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), done)
            .await
            .unwrap()
            .unwrap();
        assert!(result.unwrap_err().contains("connection changed"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn actual_disconnect_fails_a_backpressured_send() {
        let (handle, far, dir) = spawn_on_pty("write-disconnect");
        let (_, mut stream) = handle.attach();
        recv_until(&mut stream, b"serial connected").await;
        let writer = handle.clone();
        let writing = tokio::spawn(async move {
            writer
                .write_paced(Bytes::from(vec![b'x'; 64 * 1024]), Duration::ZERO)
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!writing.is_finished());
        drop(far);
        assert!(tokio::time::timeout(Duration::from_secs(3), writing)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(!handle.status().connected);
        assert!(handle
            .write_paced(Bytes::from_static(b"reboot\n"), Duration::ZERO)
            .await
            .is_err());
        std::fs::remove_dir_all(dir).ok();
    }
}
