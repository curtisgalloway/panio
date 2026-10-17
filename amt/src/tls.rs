// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

//! Give ureq's standard rustls connector an absolute socket-I/O deadline.
//! ureq 2 sets a per-read timeout before complete_io; a slow peer can keep
//! that handshake alive indefinitely. This wrapper refreshes the remaining
//! budget before every socket operation, without changing verification.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ureq::{ReadWrite, TlsConnector};

pub struct DeadlineTls {
    config: Arc<rustls::ClientConfig>,
    deadline: Arc<Mutex<Instant>>,
}

impl DeadlineTls {
    pub fn new(config: rustls::ClientConfig) -> Self {
        Self {
            config: Arc::new(config),
            deadline: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// The client serializes calls; pooled sockets use the current call's budget.
    pub fn set_deadline(&self, deadline: Instant) {
        *self.deadline.lock().expect("TLS deadline lock poisoned") = deadline;
    }
}

impl TlsConnector for DeadlineTls {
    fn connect(
        &self,
        name: &str,
        stream: Box<dyn ReadWrite>,
    ) -> Result<Box<dyn ReadWrite>, ureq::Error> {
        self.config.connect(
            name,
            Box::new(DeadlineIo {
                stream,
                deadline: self.deadline.clone(),
            }),
        )
    }
}

#[derive(Debug)]
struct DeadlineIo {
    stream: Box<dyn ReadWrite>,
    deadline: Arc<Mutex<Instant>>,
}

impl DeadlineIo {
    fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .lock()
            .expect("TLS deadline lock poisoned")
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "AMT TLS I/O deadline exceeded"))
    }

    fn prepare(&self, write: bool) -> io::Result<()> {
        let remaining = self.remaining()?;
        let socket = self.socket().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "AMT TLS requires a TCP socket with deadline support",
            )
        })?;
        if write {
            socket.set_write_timeout(Some(remaining))
        } else {
            socket.set_read_timeout(Some(remaining))
        }
    }
}

impl ReadWrite for DeadlineIo {
    fn socket(&self) -> Option<&TcpStream> {
        self.stream.socket()
    }
}

impl Read for DeadlineIo {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.prepare(false)?;
        self.stream.read(buffer)
    }
}

impl Write for DeadlineIo {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.prepare(true)?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.prepare(true)?;
        self.stream.flush()
    }
}
