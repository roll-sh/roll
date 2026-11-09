//! SaslHandshake and SaslAuthenticate (PLAIN, SCRAM-SHA-256/512). A successful exchange binds
//! the connection to the credential's stream; SASL v0 (raw tokens after the handshake) is
//! supported for old clients.

use super::{Handled, Reply, now, s};
use crate::auth::{self, Mechanism, Step};
use crate::broker::Broker;
use crate::server::Session;
use bytes::Bytes;
use kafka_protocol::ResponseError;
use kafka_protocol::messages::{SaslAuthenticateRequest, SaslAuthenticateResponse, SaslHandshakeRequest, SaslHandshakeResponse};
use std::sync::Arc;

pub fn handshake(session: &mut Session, version: i16, req: SaslHandshakeRequest) -> Handled {
    let mechanisms = auth::MECHANISMS.iter().map(|m| s(*m)).collect();
    let mut resp = SaslHandshakeResponse::default().with_mechanisms(mechanisms);
    match Mechanism::parse(&req.mechanism) {
        Some(m) => {
            session.mechanism = Some(m);
            session.sasl = Some(auth::Session::new(m));
            session.raw_sasl = version == 0;
        }
        None => resp.error_code = ResponseError::UnsupportedSaslMechanism.code(),
    }
    now(resp)
}

enum Outcome {
    Continue(Vec<u8>),
    Done(Vec<u8>),
    Fail(String),
}

fn step(b: &Broker, session: &mut Session, token: &[u8]) -> Outcome {
    let Some(sasl) = session.sasl.as_mut() else { return Outcome::Fail("SaslHandshake required first".into()) };
    match sasl.step(token, |u| b.credential(u), &b.plain) {
        Step::Continue(c) => Outcome::Continue(c),
        Step::Done { username, response } => {
            let stream = b.credential(&username).and_then(|c| b.stream(&c.stream_id));
            let Some(stream) = stream else { return Outcome::Fail("Authentication failed: the stream no longer exists".into()) };
            session.stream = Some(stream);
            session.principal = Some(username);
            session.sasl = None;
            session.raw_sasl = false;
            Outcome::Done(response)
        }
        Step::Fail(msg) => {
            tracing::debug!(peer = %session.peer, %msg, "SASL authentication failed");
            session.sasl = None;
            Outcome::Fail(msg)
        }
    }
}

pub fn authenticate(b: &Arc<Broker>, session: &mut Session, req: SaslAuthenticateRequest) -> Handled {
    let mut resp = SaslAuthenticateResponse::default();
    match step(b, session, &req.auth_bytes) {
        Outcome::Continue(c) => resp.auth_bytes = Bytes::from(c),
        Outcome::Done(r) => resp.auth_bytes = Bytes::from(r),
        Outcome::Fail(msg) => {
            resp.error_code = ResponseError::SaslAuthenticationFailed.code();
            resp.error_message = Some(s(msg));
            // Kafka sends the error and then closes; the connection drops the session's stream
            // so nothing else gets through either way.
            session.stream = None;
            return Handled { reply: Reply::Now(resp.into()), throttle_ms: 0 };
        }
    }
    now(resp)
}

/// A raw (SASL v0) token: the reply to frame back, or `None` to close the connection.
pub fn raw_token(b: &Broker, session: &mut Session, frame: &[u8]) -> Option<Bytes> {
    match step(b, session, frame) {
        Outcome::Continue(c) | Outcome::Done(c) => Some(Bytes::from(c)),
        Outcome::Fail(_) => None,
    }
}
