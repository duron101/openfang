//! SimControlChannel — the simulation *lifecycle* control plane.
//!
//! This plane is **separate from the tactical data plane**. Tactical state
//! (`WorldSnapshot`) and commands (`PlatformCommand`) always travel as protobuf
//! over the adapter; this channel carries ONLY scenario lifecycle control
//! (load/start/pause/resume/stop/reset/step/advance) as JSON, intended for an
//! experiment harness over ZMQ ROUTER/DEALER. It is **never** exposed as an
//! Agent tool — the Agent must remain unaware of simulation lifecycle control
//! so that the same decision logic runs identically against real hardware.
//!
//! Because a malicious or buggy `reset`/`stop` would destroy experiment
//! integrity, the channel is authenticated and replay-protected:
//! - **HMAC-SHA256** signature over a canonical request string,
//! - strictly increasing per-session **nonce** (replay rejection),
//! - **session id** binding (commands must target an active session),
//! - **idempotency** via `req_id` de-duplication,
//! - a lifecycle **state machine** that rejects invalid transitions,
//! - an **audit** trail of every accepted/rejected request.
//!
//! This module implements the protocol + security core transport-agnostically;
//! the ZMQ socket wiring lives behind [`SimControlTransport`] so the security
//! logic is unit-testable without a live broker.

use std::collections::{HashMap, HashSet};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

// ─────────────────────────────────────────────
// Lifecycle state machine
// ─────────────────────────────────────────────

/// Lifecycle state of a simulation instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimLifecycle {
    Uninitialized,
    Running,
    Paused,
    Stopped,
}

// ─────────────────────────────────────────────
// Control operations
// ─────────────────────────────────────────────

/// A simulation lifecycle operation (maps to the ArkSIM controller `fn`s).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "fn", rename_all = "snake_case")]
pub enum SimOp {
    /// Load + start a scenario. Issues a new session.
    Start {
        #[serde(default)]
        scenarios: Vec<String>,
        #[serde(default)]
        realtime: bool,
        #[serde(default)]
        random_seed: i64,
    },
    Pause,
    Resume,
    /// Stop / exit the instance.
    Stop,
    /// Reset / restart the instance to its initial state.
    Reset,
    /// Step the simulation by `step` frames (only when paused).
    RunStep {
        step: u32,
    },
    /// Advance to an absolute sim time.
    AdvanceToTime {
        time: f64,
    },
}

impl SimOp {
    fn tag(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Stop => "stop",
            Self::Reset => "reset",
            Self::RunStep { .. } => "runstep",
            Self::AdvanceToTime { .. } => "advance_to_time",
        }
    }

    /// Canonical, deterministic serialization of the operation arguments.
    fn args_canonical(&self) -> String {
        match self {
            Self::Start {
                scenarios,
                realtime,
                random_seed,
            } => {
                format!("{}|{}|{}", scenarios.join(","), realtime, random_seed)
            }
            Self::RunStep { step } => step.to_string(),
            Self::AdvanceToTime { time } => format!("{time}"),
            _ => String::new(),
        }
    }
}

// ─────────────────────────────────────────────
// Wire envelope
// ─────────────────────────────────────────────

/// A signed control request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimControlRequest {
    /// Unique request id (idempotency key).
    pub req_id: String,
    /// Target session id. Empty for `Start` (server issues one).
    #[serde(default)]
    pub session: String,
    /// Strictly increasing per-session nonce (replay protection).
    pub nonce: u64,
    /// HMAC-SHA256 (hex) over the canonical request string.
    pub sig: String,
    /// The operation.
    #[serde(flatten)]
    pub op: SimOp,
}

impl SimControlRequest {
    fn canonical(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.req_id,
            self.session,
            self.nonce,
            self.op.tag(),
            self.op.args_canonical()
        )
    }
}

/// Result of handling a control request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimControlResponse {
    pub req_id: String,
    pub ok: bool,
    pub session: String,
    pub state: SimLifecycle,
    pub message: String,
}

/// Reason a request was rejected.
#[derive(Debug, Clone, PartialEq)]
pub enum SimControlError {
    Malformed(String),
    BadSignature,
    ReplayedNonce {
        got: u64,
        last: u64,
    },
    UnknownSession,
    InvalidTransition {
        from: SimLifecycle,
        op: &'static str,
    },
}

impl std::fmt::Display for SimControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(e) => write!(f, "malformed: {e}"),
            Self::BadSignature => write!(f, "bad signature"),
            Self::ReplayedNonce { got, last } => write!(f, "replayed nonce {got} <= {last}"),
            Self::UnknownSession => write!(f, "unknown session"),
            Self::InvalidTransition { from, op } => {
                write!(f, "invalid transition {from:?} via {op}")
            }
        }
    }
}

// ─────────────────────────────────────────────
// Audit sink
// ─────────────────────────────────────────────

/// A sink for control-plane audit records. The kernel may bridge this to the
/// Merkle audit log; the default keeps an in-memory list.
pub trait SimAudit: Send + Sync {
    fn record(&mut self, req_id: &str, op: &str, outcome: &str);
}

/// Simple in-memory audit trail.
#[derive(Default)]
pub struct MemAudit {
    pub entries: Vec<(String, String, String)>,
}

impl SimAudit for MemAudit {
    fn record(&mut self, req_id: &str, op: &str, outcome: &str) {
        self.entries
            .push((req_id.to_string(), op.to_string(), outcome.to_string()));
    }
}

// ─────────────────────────────────────────────
// Server
// ─────────────────────────────────────────────

struct Session {
    state: SimLifecycle,
    last_nonce: u64,
}

/// The authenticated sim-control server core (transport-agnostic).
pub struct SimControlServer<A: SimAudit = MemAudit> {
    key: Vec<u8>,
    sessions: HashMap<String, Session>,
    seen_req_ids: HashSet<String>,
    cached: HashMap<String, SimControlResponse>,
    audit: A,
}

impl SimControlServer<MemAudit> {
    /// New server with the shared HMAC key and an in-memory audit trail.
    pub fn new(key: impl Into<Vec<u8>>) -> Self {
        Self::with_audit(key, MemAudit::default())
    }
}

impl<A: SimAudit> SimControlServer<A> {
    pub fn with_audit(key: impl Into<Vec<u8>>, audit: A) -> Self {
        Self {
            key: key.into(),
            sessions: HashMap::new(),
            seen_req_ids: HashSet::new(),
            cached: HashMap::new(),
            audit,
        }
    }

    pub fn audit(&self) -> &A {
        &self.audit
    }

    /// Lifecycle state of a session, if it exists.
    pub fn session_state(&self, session: &str) -> Option<SimLifecycle> {
        self.sessions.get(session).map(|s| s.state)
    }

    /// Handle a raw JSON request, returning a JSON response string.
    pub fn handle_json(&mut self, raw: &str) -> String {
        let resp = match serde_json::from_str::<SimControlRequest>(raw) {
            Ok(req) => match self.handle(req) {
                Ok(r) => r,
                Err(e) => self.error_response("", "", SimLifecycle::Uninitialized, &e),
            },
            Err(e) => {
                self.audit.record("?", "parse", "malformed");
                self.error_response(
                    "",
                    "",
                    SimLifecycle::Uninitialized,
                    &SimControlError::Malformed(e.to_string()),
                )
            }
        };
        serde_json::to_string(&resp).unwrap_or_else(|_| "{\"ok\":false}".into())
    }

    /// Handle a parsed request through the full security + state pipeline.
    pub fn handle(
        &mut self,
        req: SimControlRequest,
    ) -> Result<SimControlResponse, SimControlError> {
        // 1. Signature.
        if !self.verify(&req) {
            self.audit
                .record(&req.req_id, req.op.tag(), "bad_signature");
            return Err(SimControlError::BadSignature);
        }

        // 2. Idempotency — replay of an already-applied req_id returns the cache.
        if let Some(cached) = self.cached.get(&req.req_id) {
            self.audit
                .record(&req.req_id, req.op.tag(), "idempotent_replay");
            return Ok(cached.clone());
        }

        // 3. Start issues a fresh session; everything else binds to an existing one.
        let session_id = if matches!(req.op, SimOp::Start { .. }) {
            let id = if req.session.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                req.session.clone()
            };
            self.sessions.entry(id.clone()).or_insert(Session {
                state: SimLifecycle::Uninitialized,
                last_nonce: 0,
            });
            id
        } else {
            if !self.sessions.contains_key(&req.session) {
                self.audit
                    .record(&req.req_id, req.op.tag(), "unknown_session");
                return Err(SimControlError::UnknownSession);
            }
            req.session.clone()
        };

        // 4. Nonce replay protection (strictly increasing per session).
        {
            let s = self.sessions.get(&session_id).unwrap();
            if req.nonce <= s.last_nonce {
                self.audit
                    .record(&req.req_id, req.op.tag(), "replayed_nonce");
                return Err(SimControlError::ReplayedNonce {
                    got: req.nonce,
                    last: s.last_nonce,
                });
            }
        }

        // 5. State transition.
        let from = self.sessions.get(&session_id).unwrap().state;
        let to = match next_state(from, &req.op) {
            Some(s) => s,
            None => {
                self.audit
                    .record(&req.req_id, req.op.tag(), "invalid_transition");
                return Err(SimControlError::InvalidTransition {
                    from,
                    op: req.op.tag(),
                });
            }
        };

        // 6. Commit.
        {
            let s = self.sessions.get_mut(&session_id).unwrap();
            s.state = to;
            s.last_nonce = req.nonce;
        }
        self.seen_req_ids.insert(req.req_id.clone());

        let resp = SimControlResponse {
            req_id: req.req_id.clone(),
            ok: true,
            session: session_id,
            state: to,
            message: format!("{} -> {:?}", req.op.tag(), to),
        };
        self.cached.insert(req.req_id.clone(), resp.clone());
        self.audit.record(&req.req_id, req.op.tag(), "accepted");
        Ok(resp)
    }

    fn verify(&self, req: &SimControlRequest) -> bool {
        let expected = sign(&self.key, &req.canonical());
        // Constant-time-ish comparison via hex equality of fixed-length strings.
        expected.len() == req.sig.len()
            && expected
                .as_bytes()
                .iter()
                .zip(req.sig.as_bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    }

    fn error_response(
        &self,
        req_id: &str,
        session: &str,
        state: SimLifecycle,
        err: &SimControlError,
    ) -> SimControlResponse {
        SimControlResponse {
            req_id: req_id.to_string(),
            ok: false,
            session: session.to_string(),
            state,
            message: err.to_string(),
        }
    }
}

/// Compute the HMAC-SHA256 signature (hex) of a canonical string.
pub fn sign(key: &[u8], canonical: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac key");
    mac.update(canonical.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Build a fully-signed request (for clients / tests).
pub fn signed_request(
    key: &[u8],
    req_id: &str,
    session: &str,
    nonce: u64,
    op: SimOp,
) -> SimControlRequest {
    let mut req = SimControlRequest {
        req_id: req_id.to_string(),
        session: session.to_string(),
        nonce,
        sig: String::new(),
        op,
    };
    req.sig = sign(key, &req.canonical());
    req
}

/// Lifecycle transition table. Returns the next state, or None if invalid.
fn next_state(from: SimLifecycle, op: &SimOp) -> Option<SimLifecycle> {
    use SimLifecycle::*;
    match (from, op) {
        (Uninitialized, SimOp::Start { .. }) => Some(Running),
        (Running, SimOp::Pause) => Some(Paused),
        (Paused, SimOp::Resume) => Some(Running),
        (Running, SimOp::AdvanceToTime { .. }) => Some(Running),
        (Paused, SimOp::RunStep { .. }) => Some(Paused),
        (Paused, SimOp::AdvanceToTime { .. }) => Some(Paused),
        (Running | Paused, SimOp::Stop) => Some(Stopped),
        // Reset returns any non-uninitialized instance to Running.
        (Running | Paused | Stopped, SimOp::Reset) => Some(Running),
        _ => None,
    }
}

// ─────────────────────────────────────────────
// Transport
// ─────────────────────────────────────────────

/// Transport abstraction for the control plane (ZMQ ROUTER/DEALER in
/// deployment). Kept behind a trait so the security core needs no live broker.
pub trait SimControlTransport {
    /// Receive the next raw JSON request, if any (non-blocking).
    fn recv(&mut self) -> Option<String>;
    /// Send a raw JSON response back to the requester.
    fn send(&mut self, response: &str);
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"test-shared-secret";

    fn start(nonce: u64, req_id: &str) -> SimControlRequest {
        signed_request(
            KEY,
            req_id,
            "",
            nonce,
            SimOp::Start {
                scenarios: vec!["s.txt".into()],
                realtime: false,
                random_seed: 0,
            },
        )
    }

    #[test]
    fn start_pause_resume_step_stop_flow() {
        let mut srv = SimControlServer::new(KEY);
        let r = srv.handle(start(1, "r1")).unwrap();
        let session = r.session.clone();
        assert_eq!(r.state, SimLifecycle::Running);

        let pause = signed_request(KEY, "r2", &session, 2, SimOp::Pause);
        assert_eq!(srv.handle(pause).unwrap().state, SimLifecycle::Paused);

        let step = signed_request(KEY, "r3", &session, 3, SimOp::RunStep { step: 5 });
        assert_eq!(srv.handle(step).unwrap().state, SimLifecycle::Paused);

        let resume = signed_request(KEY, "r4", &session, 4, SimOp::Resume);
        assert_eq!(srv.handle(resume).unwrap().state, SimLifecycle::Running);

        let stop = signed_request(KEY, "r5", &session, 5, SimOp::Stop);
        assert_eq!(srv.handle(stop).unwrap().state, SimLifecycle::Stopped);

        let reset = signed_request(KEY, "r6", &session, 6, SimOp::Reset);
        assert_eq!(srv.handle(reset).unwrap().state, SimLifecycle::Running);
    }

    #[test]
    fn bad_signature_rejected() {
        let mut srv = SimControlServer::new(KEY);
        let mut req = start(1, "r1");
        req.sig = "deadbeef".into();
        assert_eq!(srv.handle(req), Err(SimControlError::BadSignature));
    }

    #[test]
    fn wrong_key_rejected() {
        let mut srv = SimControlServer::new(KEY);
        let req = start(1, "r1"); // signed with KEY
                                  // Server with a different key must reject it.
        let mut other = SimControlServer::new(b"other-key".to_vec());
        assert_eq!(
            other.handle(req.clone()),
            Err(SimControlError::BadSignature)
        );
        // Sanity: correct server accepts.
        assert!(srv.handle(req).is_ok());
    }

    #[test]
    fn replayed_nonce_rejected() {
        let mut srv = SimControlServer::new(KEY);
        let session = srv.handle(start(5, "r1")).unwrap().session;
        // A pause at nonce 3 (<= last 5) must be rejected.
        let stale = signed_request(KEY, "r2", &session, 3, SimOp::Pause);
        assert!(matches!(
            srv.handle(stale),
            Err(SimControlError::ReplayedNonce { .. })
        ));
    }

    #[test]
    fn idempotent_replay_returns_cached() {
        let mut srv = SimControlServer::new(KEY);
        let session = srv.handle(start(1, "r1")).unwrap().session;
        let pause = signed_request(KEY, "r2", &session, 2, SimOp::Pause);
        let first = srv.handle(pause.clone()).unwrap();
        // Exact same signed request again → cached response, state unchanged.
        let second = srv.handle(pause).unwrap();
        assert_eq!(first, second);
        assert_eq!(srv.session_state(&session), Some(SimLifecycle::Paused));
    }

    #[test]
    fn unknown_session_rejected() {
        let mut srv = SimControlServer::new(KEY);
        let pause = signed_request(KEY, "r1", "no-such-session", 1, SimOp::Pause);
        assert_eq!(srv.handle(pause), Err(SimControlError::UnknownSession));
    }

    #[test]
    fn invalid_transition_rejected() {
        let mut srv = SimControlServer::new(KEY);
        let session = srv.handle(start(1, "r1")).unwrap().session;
        // Resume while Running is invalid.
        let resume = signed_request(KEY, "r2", &session, 2, SimOp::Resume);
        assert!(matches!(
            srv.handle(resume),
            Err(SimControlError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn json_roundtrip() {
        let mut srv = SimControlServer::new(KEY);
        let raw = serde_json::to_string(&start(1, "r1")).unwrap();
        let resp_json = srv.handle_json(&raw);
        let resp: SimControlResponse = serde_json::from_str(&resp_json).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.state, SimLifecycle::Running);
    }

    #[test]
    fn audit_records_accept_and_reject() {
        let mut srv = SimControlServer::new(KEY);
        srv.handle(start(1, "r1")).unwrap();
        let mut bad = start(1, "rX");
        bad.sig = "00".into();
        let _ = srv.handle(bad);
        let outcomes: Vec<&str> = srv
            .audit()
            .entries
            .iter()
            .map(|(_, _, o)| o.as_str())
            .collect();
        assert!(outcomes.contains(&"accepted"));
        assert!(outcomes.contains(&"bad_signature"));
    }
}
