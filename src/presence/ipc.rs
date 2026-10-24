//! Discord's IPC pipe, with every wait for a reply bounded. The pipe is
//! opened for synchronous I/O, where a read waits for as long as Discord
//! takes to answer, so one reply that never comes would freeze the worker on
//! the activity it last sent. Replies are polled for instead, and a silent
//! Discord counts as a lost connection.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::windows::io::AsRawHandle,
    thread,
    time::{Duration, Instant},
};

use discord_rich_presence::activity::Activity;
use serde::Serialize;
use serde_json::Value;
use windows_sys::Win32::System::Pipes::PeekNamedPipe;

/// Discord answers within milliseconds when it is answering at all.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(10);
/// Discord's own replies are a few kilobytes at most.
const MAX_FRAME: usize = 64 * 1024;
const HEADER_LEN: usize = 8;
const PIPES: u32 = 10;

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
pub(super) const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;

#[derive(Serialize)]
struct Handshake<'a> {
    v: u32,
    client_id: &'a str,
}

#[derive(Serialize)]
struct Command<'a, A> {
    cmd: &'a str,
    args: A,
    nonce: &'a str,
}

#[derive(Serialize)]
struct SetActivity<'a> {
    pid: u32,
    activity: Option<&'a Activity<'a>>,
}

pub(super) struct Ipc {
    pipe: File,
    nonce: u64,
}

impl Ipc {
    /// Opens the first of Discord's pipes and completes the handshake.
    pub(super) fn connect(client_id: &str) -> io::Result<Self> {
        let pipe = (0..PIPES)
            .find_map(|i| {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(format!(r"\\?\pipe\discord-ipc-{i}"))
                    .ok()
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Discord is not running"))?;
        let mut ipc = Self { pipe, nonce: 0 };
        ipc.send(OP_HANDSHAKE, &Handshake { v: 1, client_id })?;
        // Discord answers a handshake with its READY event, or closes.
        match ipc.reply(|_, _| true)? {
            (OP_CLOSE, body) => Err(io::Error::other(format!("handshake refused: {body}"))),
            _ => Ok(ipc),
        }
    }

    /// Shows `activity`, or clears it for `None`, and returns Discord's
    /// reply.
    pub(super) fn set_activity(&mut self, activity: Option<&Activity>) -> io::Result<(u32, Value)> {
        self.nonce += 1;
        let nonce = self.nonce.to_string();
        let args = SetActivity {
            pid: std::process::id(),
            activity,
        };
        self.send(
            OP_FRAME,
            &Command {
                cmd: "SET_ACTIVITY",
                args,
                nonce: &nonce,
            },
        )?;
        // Matched by nonce, so a stray event cannot put replies out of step.
        self.reply(|_, body| body["nonce"] == nonce.as_str())
    }

    /// Checks Discord is still answering.
    pub(super) fn ping(&mut self) -> io::Result<(u32, Value)> {
        self.send(OP_PING, &())?;
        self.reply(|op, _| op == OP_PONG)
    }

    /// Writes one frame, serialising `payload` straight into it.
    fn send(&mut self, opcode: u32, payload: &impl Serialize) -> io::Result<()> {
        let mut frame = vec![0; HEADER_LEN];
        serde_json::to_writer(&mut frame, payload)?;
        let len = u32::try_from(frame.len() - HEADER_LEN)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "oversized frame"))?;
        frame[..4].copy_from_slice(&opcode.to_le_bytes());
        frame[4..HEADER_LEN].copy_from_slice(&len.to_le_bytes());
        self.pipe.write_all(&frame)
    }

    /// Reads frames until one `is_reply`, or Discord closes the connection,
    /// giving up after `REPLY_TIMEOUT`.
    fn reply(&mut self, is_reply: impl Fn(u32, &Value) -> bool) -> io::Result<(u32, Value)> {
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let mut header = [0; HEADER_LEN];
            self.read_by(&mut header, deadline)?;
            let [o0, o1, o2, o3, l0, l1, l2, l3] = header;
            let op = u32::from_le_bytes([o0, o1, o2, o3]);
            let len = u32::from_le_bytes([l0, l1, l2, l3]) as usize;
            if len > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "oversized frame",
                ));
            }
            let mut body = vec![0; len];
            self.read_by(&mut body, deadline)?;
            let body: Value = serde_json::from_slice(&body)?;
            if op == OP_CLOSE || is_reply(op, &body) {
                return Ok((op, body));
            }
        }
    }

    /// Fills `buf` once that much has arrived, never blocking past
    /// `deadline`.
    fn read_by(&mut self, buf: &mut [u8], deadline: Instant) -> io::Result<()> {
        while self.available()? < buf.len() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Discord stopped answering",
                ));
            }
            thread::sleep(POLL);
        }
        self.pipe.read_exact(buf)
    }

    /// Bytes waiting in the pipe. Fails once Discord has closed its end.
    fn available(&self) -> io::Result<usize> {
        let mut available = 0;
        // SAFETY: the handle is the open pipe `self.pipe` owns, and the call
        // only writes the count to `available`, which outlives it; with no
        // buffer passed, nothing is read from the pipe.
        let ok = unsafe {
            PeekNamedPipe(
                self.pipe.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(available as usize)
    }
}
