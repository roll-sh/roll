//! The wire: binary WebSocket messages `[kind u8][stream u32 BE][payload]`.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The protocol version a hello carries; rolld refuses another.
pub const VERSION: u32 = 1;
/// The most one WebSocket message may hold (both sides set it as the WebSocket's limit).
pub const MAX_MESSAGE: usize = 64 * 1024;
/// The most one `data` frame carries.
pub const MAX_DATA: usize = 16 * 1024;
/// Bytes a stream's sender may have in flight in each direction before the reader takes them.
pub const WINDOW: usize = 256 * 1024;
/// Streams open at once on one link.
pub const MAX_STREAMS: usize = 32;
/// The longest a stream lives (a conversation's 15 s, connecting included, plus a margin).
pub const STREAM_LIFETIME: Duration = Duration::from_secs(20);
/// How long `open` waits for the gateway's answer (it dials meanwhile).
pub const OPEN_WAIT: Duration = Duration::from_secs(10);
/// How long the gateway's dial may take.
pub const DIAL_WAIT: Duration = Duration::from_secs(8);
/// Pings both ways this often; [`SILENCE`] without a message ends the link.
pub const PING_EVERY: Duration = Duration::from_secs(20);
pub const SILENCE: Duration = Duration::from_secs(60);
/// The most an `open` payload, a hello or a reason may hold.
pub const MAX_CONTROL: usize = 8 * 1024;
/// Entries a gateway's allow-list may have.
pub const MAX_ALLOW: usize = 256;

/// Close codes. 4401: the credential no longer works, or the gateway was deleted (an identity
/// logs in again; a token stops). 4409: a newer link replaced this one. 4400: a protocol error.
/// 1012: the gateway's lease moved to another node (reconnect).
pub const CLOSE_REVOKED: u16 = 4401;
pub const CLOSE_REPLACED: u16 = 4409;
pub const CLOSE_PROTOCOL: u16 = 4400;
pub const CLOSE_MOVED: u16 = 1012;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// gateway → roll, stream 0, JSON [`Hello`]: the first message.
    Hello = 1,
    /// roll → gateway, stream 0, JSON [`Welcome`].
    Welcome = 2,
    /// roll → gateway, JSON [`Open`]: dial host:port as a new stream.
    Open = 3,
    /// gateway → roll: the stream is connected.
    Opened = 4,
    /// gateway → roll: the stream wasn't opened; the payload is why (text).
    Refused = 5,
    /// Both ways: bytes of the stream.
    Data = 6,
    /// Both ways: 4 bytes BE, more credit for the other side to send.
    Window = 7,
    /// Both ways: the sender won't send more on this stream.
    Fin = 8,
    /// Both ways: the stream is over (a reason, text).
    Reset = 9,
}

impl Kind {
    fn of(b: u8) -> Option<Kind> {
        Some(match b {
            1 => Kind::Hello,
            2 => Kind::Welcome,
            3 => Kind::Open,
            4 => Kind::Opened,
            5 => Kind::Refused,
            6 => Kind::Data,
            7 => Kind::Window,
            8 => Kind::Fin,
            9 => Kind::Reset,
            _ => return None,
        })
    }
}

/// One frame, borrowing its payload from the message.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub kind: Kind,
    pub stream: u32,
    pub payload: &'a [u8],
}

pub fn encode(kind: Kind, stream: u32, payload: &[u8]) -> bytes::Bytes {
    let mut b = bytes::BytesMut::with_capacity(5 + payload.len());
    b.extend_from_slice(&[kind as u8]);
    b.extend_from_slice(&stream.to_be_bytes());
    b.extend_from_slice(payload);
    b.freeze()
}

/// A message's frame, checked against the limits (control payloads at most [`MAX_CONTROL`],
/// data at most [`MAX_DATA`] and never empty, a window exactly 4 bytes).
pub fn decode(msg: &[u8]) -> Result<Frame<'_>, String> {
    if msg.len() < 5 || msg.len() > MAX_MESSAGE {
        return Err("a frame is 5 bytes to 64 KiB".into());
    }
    let kind = Kind::of(msg[0]).ok_or_else(|| format!("unknown frame kind {}", msg[0]))?;
    let stream = u32::from_be_bytes([msg[1], msg[2], msg[3], msg[4]]);
    let payload = &msg[5..];
    let ok = match kind {
        Kind::Data => !payload.is_empty() && payload.len() <= MAX_DATA,
        Kind::Window => payload.len() == 4,
        Kind::Opened | Kind::Fin => payload.is_empty(),
        _ => payload.len() <= MAX_CONTROL,
    };
    if !ok {
        return Err(format!("a {kind:?} frame of {} bytes", payload.len()));
    }
    let control = matches!(kind, Kind::Hello | Kind::Welcome);
    if control != (stream == 0) {
        return Err(format!("a {kind:?} frame on stream {stream}"));
    }
    Ok(Frame { kind, stream, payload })
}

/// The gateway's first message: what it is and what it reaches (shown in roll, never used to
/// widen anything: the gateway enforces its list itself).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
    /// The roll CLI's version running it.
    pub agent: String,
    pub allow: Vec<String>,
    pub max_streams: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Welcome {
    pub gateway: String,
    pub node: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Open {
    pub host: String,
    pub port: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_hostile_ones_are_refused() {
        let m = encode(Kind::Data, 7, b"hi");
        assert_eq!(decode(&m).unwrap(), Frame { kind: Kind::Data, stream: 7, payload: b"hi" });
        assert!(decode(&encode(Kind::Data, 7, b"")).is_err(), "empty data");
        assert!(decode(&encode(Kind::Data, 7, &vec![0; MAX_DATA + 1])).is_err());
        assert!(decode(&encode(Kind::Window, 7, &[0, 0, 1])).is_err());
        assert!(decode(&encode(Kind::Hello, 3, b"{}")).is_err(), "hello is stream 0");
        assert!(decode(&encode(Kind::Open, 0, b"{}")).is_err(), "a stream is never 0");
        assert!(decode(&[42, 0, 0, 0, 1]).is_err());
        assert!(decode(&[6, 0]).is_err());
    }
}
