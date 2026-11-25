//! The wire protocol between [`BwrapSandbox`](crate::bwrap::BwrapSandbox) and `nixxa-sandboxd`.
//!
//! Frames go over the daemon's stdin (client → daemon) and stdout (daemon → client): a 4-byte
//! big-endian length, then that many bytes of JSON. A frame larger than [`MAX_FRAME`] is refused
//! before its body is read. The daemon's stderr carries its logs only (metadata, never output or
//! command text), which the client forwards to `tracing`.
//!
//! The session starts with a handshake: the client sends [`ToDaemon::Hello`] with the protocol
//! version and the limits to enforce, and the daemon answers [`FromDaemon::Hello`] with its own
//! version, or exits. After that the client sends [`ToDaemon::Request`]s, each with an id, and
//! the daemon answers each with a [`FromDaemon::Reply`] carrying the same id. While an `Exec` is
//! running the client may send [`ToDaemon::Cancel`] for it (or a `Reset`); the daemon kills the
//! command and still replies to it.
//!
//! The user's terminal (version 4): [`Op::TerminalOpen`] starts it; its output comes as
//! [`FromDaemon::TerminalOutput`] frames, then one [`FromDaemon::TerminalClosed`]. The daemon
//! sends at most a window (64 KiB) of output the client hasn't acknowledged
//! ([`ToDaemon::TerminalAck`], sent as the output is consumed), and stops reading the PTY until
//! more is acknowledged: a slow reader makes the shell wait, and nothing is dropped.
//!
//! This file is compiled into both the library and the daemon binary.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use nixxa_contracts::{Outcome, SandboxAction, SandboxEvent, TerminalSize};

/// The protocol version both ends must agree on. 5: `Patch`, and reads carry the file's hash.
pub(crate) const VERSION: u32 = 5;

/// The largest frame either end sends or accepts, in bytes (the JSON body, without the prefix).
pub(crate) const MAX_FRAME: usize = 8 * 1024 * 1024;

/// Resource limits the daemon enforces on every command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Limits {
    /// Time limit when an `Exec` asks for none, in milliseconds.
    pub default_timeout_ms: u64,
    /// The most time any one `Exec` gets, in milliseconds.
    pub max_timeout_ms: u64,
    /// Bytes kept from each of stdout and stderr; past this the command is killed.
    pub output_cap: u64,
    /// The largest file `ReadFile` returns, in bytes.
    pub read_cap: u64,
    /// The largest content `WriteFile` accepts, in bytes.
    pub write_cap: u64,
    /// Per-process rlimits applied to every command.
    pub rlimits: Rlimits,
    /// Default time limit for a background job, in milliseconds.
    pub background_default_timeout_ms: u64,
    /// The most time any background job gets, in milliseconds.
    pub background_max_timeout_ms: u64,
    /// Bytes kept from each stream of a background job.
    pub background_output_cap: u64,
    /// The most background jobs running at once.
    pub max_background_jobs: u64,
    /// Put before every job id, so a restarted daemon never reuses an earlier daemon's ids.
    pub job_prefix: String,
    /// Whether commands get the network proxy (the policy isn't `Off`).
    pub network: bool,
}

/// Per-process resource limits (`setrlimit`, soft = hard, so a command can't raise them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Rlimits {
    /// `RLIMIT_NPROC`: processes in the sandbox's user namespace.
    pub nproc: u64,
    /// `RLIMIT_FSIZE`: the largest file a command can write, in bytes.
    pub fsize: u64,
    /// `RLIMIT_NOFILE`: open file descriptors per process.
    pub nofile: u64,
    /// `RLIMIT_CPU`: CPU seconds per process.
    pub cpu_secs: u64,
    /// `RLIMIT_DATA`: data segment + private writable mappings per process, in bytes.
    pub data: u64,
}

/// Client → daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ToDaemon {
    /// The first frame: the client's version and the limits to enforce.
    Hello { version: u32, limits: Limits },
    /// Do something; answered by a [`FromDaemon::Reply`] with the same id.
    Request { id: u64, op: Op },
    /// Stop the request with this id if it is still running. It is still answered.
    Cancel { id: u64 },
    /// The client has consumed this many bytes of the terminal's output: the daemon may send
    /// that much more. Not answered.
    TerminalAck { terminal: u64, bytes: u32 },
}

/// What a [`ToDaemon::Request`] asks for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Op {
    /// Run a sandbox action.
    Action { action: SandboxAction },
    /// Kill everything running; the workspace is kept.
    Reset,
    /// Check the daemon is responsive.
    Ping,
    /// Turn the network proxy on or off for the commands started from now on.
    Network { on: bool },
    /// Open the user's terminal, with the client's id for it. Answered once it has started.
    TerminalOpen { terminal: u64, size: TerminalSize },
    /// Bytes typed into the terminal. Answered once the terminal has taken them.
    TerminalInput {
        terminal: u64,
        #[serde(with = "base64")]
        data: Vec<u8>,
    },
    /// The terminal's new size.
    TerminalResize { terminal: u64, size: TerminalSize },
    /// End the terminal and everything it started. Answered once they are gone.
    TerminalClose { terminal: u64 },
}

/// The isolation layers the daemon applied to itself at start-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ActiveLayers {
    /// Whether Landlock is enforcing a filesystem (and, where supported, TCP) policy.
    pub landlock: bool,
    /// Whether the seccomp deny-list filter is installed.
    pub seccomp: bool,
}

/// Daemon → client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum FromDaemon {
    /// The answer to [`ToDaemon::Hello`].
    Hello { version: u32, layers: ActiveLayers },
    /// The answer to the request with this id.
    Reply { id: u64, reply: Reply },
    /// An unsolicited event (job completion, network request).
    Event { event: SandboxEvent },
    /// Output of the user's terminal.
    TerminalOutput {
        terminal: u64,
        #[serde(with = "base64")]
        data: Vec<u8>,
    },
    /// The user's terminal has ended; nothing more comes from it.
    TerminalClosed { terminal: u64, reason: String },
}

/// The result of one request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub(crate) enum Reply {
    /// The action ran (successfully or not).
    Outcome { outcome: Outcome, truncated: bool },
    /// The command hit its time limit and was killed.
    Timeout,
    /// The request was cancelled and its command killed.
    Cancelled,
    /// A `Reset` or `Ping` succeeded.
    Done,
    /// The daemon could not handle the request.
    Error { message: String },
}

/// Why a frame could not be read or written.
#[derive(Debug)]
pub(crate) enum FrameError {
    /// The stream ended cleanly between frames.
    Eof,
    /// The frame is larger than [`MAX_FRAME`]. The stream can't be resynchronised after this.
    TooLarge(usize),
    /// The frame's body is not a valid message.
    Malformed(String),
    /// The stream failed (including ending mid-frame).
    Io(std::io::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eof => f.write_str("the stream ended"),
            Self::TooLarge(n) => write!(f, "a {n}-byte frame is over the {MAX_FRAME}-byte limit"),
            Self::Malformed(why) => write!(f, "malformed frame: {why}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

/// Serialises `msg` into one frame, prefix included.
pub(crate) fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(msg).map_err(|err| FrameError::Malformed(err.to_string()))?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    let len = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge(body.len()))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Parses one frame's body.
pub(crate) fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    serde_json::from_slice(body).map_err(|err| FrameError::Malformed(err.to_string()))
}

/// Writes one frame and flushes it.
pub(crate) async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<(), FrameError> {
    let frame = encode(msg)?;
    w.write_all(&frame).await.map_err(FrameError::Io)?;
    w.flush().await.map_err(FrameError::Io)
}

/// Reads one frame. Refuses an oversized one before reading its body.
pub(crate) async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<T, FrameError> {
    let mut prefix = [0u8; 4];
    let mut got = 0;
    while got < prefix.len() {
        match r.read(&mut prefix[got..]).await {
            Ok(0) if got == 0 => return Err(FrameError::Eof),
            Ok(0) => {
                return Err(FrameError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the stream ended inside a frame header",
                )));
            }
            Ok(n) => got += n,
            Err(err) => return Err(FrameError::Io(err)),
        }
    }
    let len = u32::from_be_bytes(prefix) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.map_err(FrameError::Io)?;
    decode(&body)
}

/// Terminal bytes as base64 text (JSON has no bytes, and an array of numbers is four times
/// larger).
mod base64 {
    use serde::{Deserialize, Deserializer, Serializer};

    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub(crate) fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    pub(crate) fn decode(text: &str) -> Result<Vec<u8>, String> {
        let text = text.as_bytes();
        if text.len() % 4 != 0 {
            return Err("base64 text must come in groups of four".into());
        }
        let mut out = Vec::with_capacity(text.len() / 4 * 3);
        for (index, group) in text.chunks(4).enumerate() {
            let last = index + 1 == text.len() / 4;
            let pad = group.iter().rev().take_while(|c| **c == b'=').count();
            if pad > 2 || (pad > 0 && !last) {
                return Err("misplaced base64 padding".into());
            }
            let mut n = 0u32;
            for (i, c) in group[..4 - pad].iter().enumerate() {
                let value = ALPHABET.iter().position(|a| a == c).ok_or("not base64")?;
                n |= (value as u32) << (18 - 6 * i);
            }
            out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
        }
        Ok(out)
    }

    pub(crate) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&encode(bytes))
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        decode(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            default_timeout_ms: 1,
            max_timeout_ms: 2,
            output_cap: 3,
            read_cap: 4,
            write_cap: 5,
            rlimits: Rlimits { nproc: 6, fsize: 7, nofile: 8, cpu_secs: 9, data: 10 },
            background_default_timeout_ms: 11,
            background_max_timeout_ms: 12,
            background_output_cap: 13,
            max_background_jobs: 14,
            job_prefix: "2-".into(),
            network: true,
        }
    }

    async fn round_trip<T: Serialize + DeserializeOwned>(msg: &T) -> T {
        let frame = encode(msg).expect("encode");
        read_frame(&mut frame.as_slice()).await.expect("decode")
    }

    #[tokio::test]
    async fn every_message_round_trips() {
        let to = [
            ToDaemon::Hello { version: VERSION, limits: limits() },
            ToDaemon::Request {
                id: 7,
                op: Op::Action {
                    action: SandboxAction::Exec {
                        command: "echo hi".into(),
                        cwd: Some("src".into()),
                        timeout_ms: None,
                        background: false,
                    },
                },
            },
            ToDaemon::Request { id: 8, op: Op::Reset },
            ToDaemon::Request { id: 9, op: Op::Ping },
            ToDaemon::Request { id: 10, op: Op::Network { on: true } },
            ToDaemon::Request {
                id: 11,
                op: Op::TerminalOpen { terminal: 1, size: TerminalSize { cols: 80, rows: 24 } },
            },
            ToDaemon::Request { id: 12, op: Op::TerminalInput { terminal: 1, data: b"ls\r\x03\xff".to_vec() } },
            ToDaemon::Request {
                id: 13,
                op: Op::TerminalResize { terminal: 1, size: TerminalSize { cols: 1, rows: 2 } },
            },
            ToDaemon::Request { id: 14, op: Op::TerminalClose { terminal: 1 } },
            ToDaemon::Cancel { id: 7 },
            ToDaemon::TerminalAck { terminal: 1, bytes: 4096 },
        ];
        for msg in to {
            assert_eq!(round_trip(&msg).await, msg);
        }
        let from = [
            FromDaemon::Hello { version: VERSION, layers: ActiveLayers { landlock: true, seccomp: true } },
            FromDaemon::Reply {
                id: 1,
                reply: Reply::Outcome { outcome: Outcome::Written { bytes: 3 }, truncated: false },
            },
            FromDaemon::Reply { id: 2, reply: Reply::Timeout },
            FromDaemon::Reply { id: 3, reply: Reply::Cancelled },
            FromDaemon::Reply { id: 4, reply: Reply::Done },
            FromDaemon::Reply { id: 5, reply: Reply::Error { message: "busy".into() } },
            FromDaemon::TerminalOutput { terminal: 1, data: b"\x1b]52;c;aGk=\x07".to_vec() },
            FromDaemon::TerminalClosed { terminal: 1, reason: "the shell exited".into() },
        ];
        for msg in from {
            assert_eq!(round_trip(&msg).await, msg);
        }
    }

    #[test]
    fn base64_round_trips_every_length_and_byte() {
        let all: Vec<u8> = (0..=255).collect();
        for len in 0..all.len() {
            let text = base64::encode(&all[..len]);
            assert_eq!(base64::decode(&text).unwrap(), &all[..len], "{len}");
        }
        assert_eq!(base64::encode(b"hi"), "aGk=");
        assert_eq!(base64::encode(b"nixxa"), "bml4eGE=");
        for bad in ["a", "aGk", "a===", "aG=k", "aGk=aGk=", "a!k="] {
            assert!(base64::decode(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn frames_are_length_prefixed_json() {
        let frame = encode(&ToDaemon::Cancel { id: 4 }).expect("encode");
        let body = br#"{"type":"cancel","id":4}"#;
        assert_eq!(&frame[..4], &(body.len() as u32).to_be_bytes());
        assert_eq!(&frame[4..], body);
    }

    #[tokio::test]
    async fn an_oversized_frame_is_refused_before_its_body() {
        let mut stream: &[u8] = &((MAX_FRAME as u32) + 1).to_be_bytes();
        let err = read_frame::<_, ToDaemon>(&mut stream).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(n) if n == MAX_FRAME + 1), "{err}");
        let mut stream: &[u8] = &u32::MAX.to_be_bytes();
        assert!(matches!(read_frame::<_, ToDaemon>(&mut stream).await, Err(FrameError::TooLarge(_))));
    }

    #[tokio::test]
    async fn an_oversized_message_is_not_sent() {
        let huge = ToDaemon::Request {
            id: 1,
            op: Op::Action { action: SandboxAction::WriteFile { path: "a".into(), content: "x".repeat(MAX_FRAME) } },
        };
        assert!(matches!(encode(&huge), Err(FrameError::TooLarge(_))));
    }

    #[tokio::test]
    async fn malformed_frames_are_errors_not_panics() {
        let cases: [&[u8]; 6] = [
            b"\x00\x00\x00\x05hello",                 // not JSON
            b"\x00\x00\x00\x02{}",                    // JSON, no tag
            b"\x00\x00\x00\x10{\"type\":\"nope\"}  ", // unknown tag
            b"\x00\x00\x00\x02\xff\xfe",              // not UTF-8
            b"\x00\x00\x00\x09{\"type\":",            // body shorter than its prefix
            b"\x00\x00",                              // prefix cut short
        ];
        for bytes in cases {
            let mut stream = bytes;
            let err = read_frame::<_, ToDaemon>(&mut stream).await;
            assert!(matches!(err, Err(FrameError::Malformed(_) | FrameError::Io(_))), "{bytes:?}: {err:?}");
        }
    }

    #[tokio::test]
    async fn a_clean_end_between_frames_is_eof() {
        let mut stream: &[u8] = b"";
        assert!(matches!(read_frame::<_, ToDaemon>(&mut stream).await, Err(FrameError::Eof)));
    }

    #[tokio::test]
    async fn consecutive_frames_are_read_in_order() {
        let mut bytes = encode(&ToDaemon::Cancel { id: 1 }).unwrap();
        bytes.extend(encode(&ToDaemon::Cancel { id: 2 }).unwrap());
        let mut stream = bytes.as_slice();
        assert_eq!(read_frame::<_, ToDaemon>(&mut stream).await.unwrap(), ToDaemon::Cancel { id: 1 });
        assert_eq!(read_frame::<_, ToDaemon>(&mut stream).await.unwrap(), ToDaemon::Cancel { id: 2 });
        assert!(matches!(read_frame::<_, ToDaemon>(&mut stream).await, Err(FrameError::Eof)));
    }
}
