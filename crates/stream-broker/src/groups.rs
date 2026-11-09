//! Group coordinator for the classic consumer group protocol (JoinGroup / SyncGroup / Heartbeat /
//! LeaveGroup), following Kafka's `GroupCoordinator` state machine:
//!
//! ```text
//! Empty ──join──▶ PreparingRebalance ──all joined──▶ CompletingRebalance ──leader syncs──▶ Stable
//!   ▲                    ▲  │                                  │                           │
//!   └──last member left──┘  └──────────── member joins, leaves or times out ◀──────────────┘
//! ```
//!
//! Membership lives in memory; committed offsets and the generation are persisted, so after a
//! restart members get `UNKNOWN_MEMBER_ID` and simply rejoin.

use crate::broker::Broker;
use crate::ids::{new_uuid, now, now_ms};
use crate::store::OffsetRow;
use bytes::Bytes;
use kafka_protocol::ResponseError;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tracing::debug;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Empty,
    PreparingRebalance,
    CompletingRebalance,
    Stable,
    Dead,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Empty => "Empty",
            State::PreparingRebalance => "PreparingRebalance",
            State::CompletingRebalance => "CompletingRebalance",
            State::Stable => "Stable",
            State::Dead => "Dead",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct JoinResult {
    pub error: i16,
    pub generation: i32,
    pub protocol_type: Option<String>,
    pub protocol_name: Option<String>,
    pub leader: String,
    pub member_id: String,
    /// For the leader: every member with its metadata for the chosen protocol.
    pub members: Vec<(String, Option<String>, Bytes)>,
}

impl JoinResult {
    fn error(e: ResponseError, member_id: &str) -> Self {
        Self { error: e.code(), generation: -1, member_id: member_id.into(), ..Default::default() }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SyncResult {
    pub error: i16,
    pub protocol_type: Option<String>,
    pub protocol_name: Option<String>,
    pub assignment: Bytes,
}

impl SyncResult {
    fn error(e: ResponseError) -> Self {
        Self { error: e.code(), ..Default::default() }
    }
}

pub struct JoinRequest {
    pub group_id: String,
    pub member_id: String,
    pub instance_id: Option<String>,
    pub client_id: String,
    pub client_host: String,
    pub session_timeout_ms: i32,
    pub rebalance_timeout_ms: i32,
    pub protocol_type: String,
    pub protocols: Vec<(String, Bytes)>,
    /// JoinGroup v4+ requires a member id from the coordinator first (KIP-394).
    pub require_known_member_id: bool,
}

pub struct SyncRequest {
    pub group_id: String,
    pub member_id: String,
    pub instance_id: Option<String>,
    pub generation: i32,
    pub protocol_type: Option<String>,
    pub protocol_name: Option<String>,
    pub assignments: Vec<(String, Bytes)>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct MemberInfo {
    pub member_id: String,
    pub instance_id: Option<String>,
    pub client_id: String,
    pub client_host: String,
    #[serde(skip)]
    pub metadata: Bytes,
    #[serde(skip)]
    pub assignment: Bytes,
    /// Topics from the consumer subscription (consumer protocol only).
    pub topics: Vec<String>,
    /// Assigned `(topic, partitions)` (consumer protocol only).
    pub assigned: Vec<(String, Vec<i32>)>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct GroupInfo {
    pub group_id: String,
    pub state: &'static str,
    pub protocol_type: String,
    pub protocol: String,
    pub generation: i32,
    pub leader: Option<String>,
    pub members: Vec<MemberInfo>,
}

struct Member {
    id: String,
    instance_id: Option<String>,
    client_id: String,
    client_host: String,
    session_timeout: Duration,
    rebalance_timeout: Duration,
    protocols: Vec<(String, Bytes)>,
    assignment: Bytes,
    join_waiter: Option<oneshot::Sender<JoinResult>>,
    sync_waiter: Option<oneshot::Sender<SyncResult>>,
    last_seen: Instant,
    order: u64,
}

impl Member {
    fn metadata(&self, protocol: &str) -> Bytes {
        self.protocols.iter().find(|(n, _)| n == protocol).map(|(_, m)| m.clone()).unwrap_or_default()
    }
    fn awaiting(&self) -> bool {
        self.join_waiter.is_some() || self.sync_waiter.is_some()
    }
}

struct Group {
    state: State,
    protocol_type: Option<String>,
    protocol: Option<String>,
    generation: i32,
    leader: Option<String>,
    members: HashMap<String, Member>,
    /// Member ids handed out with MEMBER_ID_REQUIRED, waiting for the rejoin.
    pending: HashMap<String, Instant>,
    statics: HashMap<String, String>,
    rebalance_deadline: Option<Instant>,
    delay_until: Option<Instant>,
    sync_deadline: Option<Instant>,
    next_order: u64,
}

impl Group {
    fn new(generation: i32, protocol_type: Option<String>) -> Self {
        Self {
            state: State::Empty,
            protocol_type,
            protocol: None,
            generation,
            leader: None,
            members: HashMap::new(),
            pending: HashMap::new(),
            statics: HashMap::new(),
            rebalance_deadline: None,
            delay_until: None,
            sync_deadline: None,
            next_order: 0,
        }
    }

    fn supports(&self, protocol_type: &str, protocols: &[(String, Bytes)]) -> bool {
        if self.members.is_empty() {
            return !protocols.is_empty();
        }
        if self.protocol_type.as_deref() != Some(protocol_type) {
            return false;
        }
        protocols.iter().any(|(p, _)| self.members.values().all(|m| m.protocols.iter().any(|(q, _)| q == p)))
    }

    fn current_join(&self, member_id: &str) -> JoinResult {
        let leader = self.leader.clone().unwrap_or_default();
        let protocol = self.protocol.clone().unwrap_or_default();
        let members = if leader == member_id {
            let mut ms: Vec<&Member> = self.members.values().collect();
            ms.sort_by_key(|m| m.order);
            ms.iter().map(|m| (m.id.clone(), m.instance_id.clone(), m.metadata(&protocol))).collect()
        } else {
            vec![]
        };
        JoinResult {
            error: 0,
            generation: self.generation,
            protocol_type: self.protocol_type.clone(),
            protocol_name: self.protocol.clone(),
            leader,
            member_id: member_id.into(),
            members,
        }
    }

    fn remove_member(&mut self, id: &str, reason: ResponseError) {
        if let Some(mut m) = self.members.remove(id) {
            if let Some(w) = m.join_waiter.take() {
                let _ = w.send(JoinResult::error(reason, id));
            }
            if let Some(w) = m.sync_waiter.take() {
                let _ = w.send(SyncResult::error(reason));
            }
            if let Some(i) = &m.instance_id
                && self.statics.get(i).is_some_and(|x| x == id)
            {
                self.statics.remove(i);
            }
            if self.leader.as_deref() == Some(id) {
                self.leader = None;
            }
        }
    }
}

type Key = (String, String);

#[derive(Default)]
pub struct Groups {
    groups: Mutex<HashMap<Key, Arc<Mutex<Group>>>>,
    /// Transactional offset commits waiting for their transaction: `(stream, group) → producer → rows`.
    pending_txn: Mutex<HashMap<Key, HashMap<i64, Vec<OffsetRow>>>>,
}

fn key(stream: &str, group: &str) -> Key {
    (stream.to_string(), group.to_string())
}

impl Groups {
    fn get(&self, stream: &str, group: &str) -> Option<Arc<Mutex<Group>>> {
        self.groups.lock().unwrap().get(&key(stream, group)).cloned()
    }

    fn get_or_create(&self, b: &Broker, stream: &str, group: &str) -> Arc<Mutex<Group>> {
        let mut m = self.groups.lock().unwrap();
        m.entry(key(stream, group))
            .or_insert_with(|| {
                let (pt, generation) = b.store.group(stream, group).ok().flatten().map(|(p, g)| (Some(p), g)).unwrap_or((None, 0));
                Arc::new(Mutex::new(Group::new(generation, pt)))
            })
            .clone()
    }

    pub fn drop_stream(&self, stream: &str) {
        self.groups.lock().unwrap().retain(|(s, _), _| s != stream);
        self.pending_txn.lock().unwrap().retain(|(s, _), _| s != stream);
    }

    pub fn topic_deleted(&self, stream: &str, topic: &str) {
        for pending in self.pending_txn.lock().unwrap().iter_mut().filter(|((s, _), _)| s == stream).map(|(_, v)| v) {
            for rows in pending.values_mut() {
                rows.retain(|r| r.topic != topic);
            }
        }
    }

    pub async fn join(&self, b: &Broker, stream: &str, req: JoinRequest) -> JoinResult {
        let rx = match self.join_now(b, stream, req) {
            Ok(r) => return r,
            Err(rx) => rx,
        };
        rx.await.unwrap_or_else(|_| JoinResult::error(ResponseError::RebalanceInProgress, ""))
    }

    fn join_now(&self, b: &Broker, stream: &str, req: JoinRequest) -> Result<JoinResult, oneshot::Receiver<JoinResult>> {
        let cfg = &b.config.groups;
        if req.group_id.is_empty() {
            return Ok(JoinResult::error(ResponseError::InvalidGroupId, &req.member_id));
        }
        if req.session_timeout_ms < cfg.min_session_timeout_ms || req.session_timeout_ms > cfg.max_session_timeout_ms {
            return Ok(JoinResult::error(ResponseError::InvalidSessionTimeout, &req.member_id));
        }
        if !req.member_id.is_empty() && self.get(stream, &req.group_id).is_none() {
            return Ok(JoinResult::error(ResponseError::UnknownMemberId, &req.member_id));
        }
        let group = self.get_or_create(b, stream, &req.group_id);
        let mut g = group.lock().unwrap();
        if g.state == State::Dead {
            return Ok(JoinResult::error(ResponseError::CoordinatorNotAvailable, &req.member_id));
        }
        if !g.supports(&req.protocol_type, &req.protocols) {
            return Ok(JoinResult::error(ResponseError::InconsistentGroupProtocol, &req.member_id));
        }
        let now = Instant::now();
        let rebalance_timeout = Duration::from_millis(req.rebalance_timeout_ms.max(req.session_timeout_ms).max(0) as u64);
        let session_timeout = Duration::from_millis(req.session_timeout_ms as u64);

        let mut member_id = req.member_id.clone();
        if member_id.is_empty() {
            if g.members.len() + g.pending.len() >= cfg.max_size {
                return Ok(JoinResult::error(ResponseError::GroupMaxSizeReached, ""));
            }
            member_id = format!("{}-{}", if req.client_id.is_empty() { "member" } else { &req.client_id }, new_uuid());
            if let Some(instance) = &req.instance_id {
                if let Some(old) = g.statics.get(instance).cloned()
                    && let Some(mut m) = g.members.remove(&old)
                {
                    // A static member restarted: it takes over its old identity and assignment.
                    if let Some(w) = m.join_waiter.take() {
                        let _ = w.send(JoinResult::error(ResponseError::FencedInstanceId, &old));
                    }
                    if let Some(w) = m.sync_waiter.take() {
                        let _ = w.send(SyncResult::error(ResponseError::FencedInstanceId));
                    }
                    let unchanged = m.protocols == req.protocols;
                    m.id = member_id.clone();
                    m.last_seen = now;
                    m.session_timeout = session_timeout;
                    m.rebalance_timeout = rebalance_timeout;
                    m.protocols = req.protocols.clone();
                    let was_leader = g.leader.as_deref() == Some(old.as_str());
                    if was_leader {
                        g.leader = Some(member_id.clone());
                    }
                    g.members.insert(member_id.clone(), m);
                    g.statics.insert(instance.clone(), member_id.clone());
                    if g.state == State::Stable && unchanged {
                        let mut r = g.current_join(&member_id);
                        if was_leader {
                            // The leader already has the assignment; don't make it compute one.
                            r.members.clear();
                        }
                        return Ok(r);
                    }
                    return Err(self.park_join(b, stream, &req.group_id, &mut g, &member_id, true));
                }
            } else if req.require_known_member_id {
                g.pending.insert(member_id.clone(), now + session_timeout);
                return Ok(JoinResult::error(ResponseError::MemberIdRequired, &member_id));
            }
            self.add_member(&mut g, &member_id, &req, session_timeout, rebalance_timeout, now);
            return Err(self.park_join(b, stream, &req.group_id, &mut g, &member_id, true));
        }

        if g.pending.remove(&member_id).is_some() {
            self.add_member(&mut g, &member_id, &req, session_timeout, rebalance_timeout, now);
            return Err(self.park_join(b, stream, &req.group_id, &mut g, &member_id, true));
        }
        if let Some(instance) = &req.instance_id
            && g.statics.get(instance).is_some_and(|m| *m != member_id)
        {
            return Ok(JoinResult::error(ResponseError::FencedInstanceId, &member_id));
        }
        let Some(m) = g.members.get_mut(&member_id) else {
            return Ok(JoinResult::error(ResponseError::UnknownMemberId, &member_id));
        };
        let changed = m.protocols != req.protocols;
        m.protocols = req.protocols.clone();
        m.session_timeout = session_timeout;
        m.rebalance_timeout = rebalance_timeout;
        m.last_seen = now;
        m.client_id = req.client_id.clone();
        let is_leader = g.leader.as_deref() == Some(member_id.as_str());
        match g.state {
            State::PreparingRebalance => Err(self.park_join(b, stream, &req.group_id, &mut g, &member_id, false)),
            State::CompletingRebalance if !changed => Ok(g.current_join(&member_id)),
            State::Stable if !changed && !is_leader => Ok(g.current_join(&member_id)),
            _ => Err(self.park_join(b, stream, &req.group_id, &mut g, &member_id, true)),
        }
    }

    fn add_member(&self, g: &mut Group, id: &str, req: &JoinRequest, session: Duration, rebalance: Duration, now: Instant) {
        g.next_order += 1;
        let order = g.next_order;
        if g.members.is_empty() {
            g.protocol_type = Some(req.protocol_type.clone());
        }
        if let Some(i) = &req.instance_id {
            g.statics.insert(i.clone(), id.to_string());
        }
        g.members.insert(
            id.to_string(),
            Member {
                id: id.to_string(),
                instance_id: req.instance_id.clone(),
                client_id: req.client_id.clone(),
                client_host: req.client_host.clone(),
                session_timeout: session,
                rebalance_timeout: rebalance,
                protocols: req.protocols.clone(),
                assignment: Bytes::new(),
                join_waiter: None,
                sync_waiter: None,
                last_seen: now,
                order,
            },
        );
    }

    /// Register the member's join waiter, starting a rebalance if needed.
    fn park_join(&self, b: &Broker, stream: &str, group_id: &str, g: &mut Group, member: &str, rebalance: bool) -> oneshot::Receiver<JoinResult> {
        let (tx, rx) = oneshot::channel();
        if let Some(m) = g.members.get_mut(member)
            && let Some(old) = m.join_waiter.replace(tx) {
                let _ = old.send(JoinResult::error(ResponseError::RebalanceInProgress, member));
            }
        if g.state != State::PreparingRebalance {
            self.prepare_rebalance(b, g);
        } else if rebalance && let Some(until) = g.delay_until {
            // More members are still arriving at an empty group: extend the initial delay.
            let extended = Instant::now() + Duration::from_millis(b.config.groups.initial_rebalance_delay_ms);
            g.delay_until = Some(extended.min(g.rebalance_deadline.unwrap_or(extended)).max(until));
        }
        self.maybe_complete_join(b, stream, group_id, g, false);
        rx
    }

    fn prepare_rebalance(&self, b: &Broker, g: &mut Group) {
        if g.state == State::CompletingRebalance {
            for m in g.members.values_mut() {
                m.assignment = Bytes::new();
                if let Some(w) = m.sync_waiter.take() {
                    let _ = w.send(SyncResult::error(ResponseError::RebalanceInProgress));
                }
            }
        }
        let now = Instant::now();
        let timeout = g.members.values().map(|m| m.rebalance_timeout).max().unwrap_or(Duration::from_secs(1));
        if g.state != State::PreparingRebalance {
            g.rebalance_deadline = Some(now + timeout);
            g.delay_until = (g.state == State::Empty)
                .then(|| now + Duration::from_millis(b.config.groups.initial_rebalance_delay_ms).min(timeout));
        }
        g.sync_deadline = None;
        g.state = State::PreparingRebalance;
    }

    /// Finish the join phase once every member has rejoined (or, on `timed_out`, without the
    /// stragglers).
    fn maybe_complete_join(&self, b: &Broker, stream: &str, group_id: &str, g: &mut Group, timed_out: bool) {
        if g.state != State::PreparingRebalance {
            return;
        }
        let now = Instant::now();
        if !timed_out {
            let all_joined = g.members.values().all(|m| m.join_waiter.is_some()) && g.pending.is_empty();
            if !all_joined || g.delay_until.is_some_and(|d| now < d) {
                return;
            }
        } else {
            let stragglers: Vec<String> = g.members.values().filter(|m| m.join_waiter.is_none()).map(|m| m.id.clone()).collect();
            for id in stragglers {
                g.remove_member(&id, ResponseError::UnknownMemberId);
            }
            g.pending.clear();
        }
        g.generation += 1;
        g.rebalance_deadline = None;
        g.delay_until = None;
        if g.members.is_empty() {
            g.state = State::Empty;
            g.protocol = None;
            g.leader = None;
            let _ = b.store.save_group(stream, group_id, g.protocol_type.as_deref().unwrap_or(""), g.generation, now_secs());
            debug!(stream, group = group_id, generation = g.generation, "group is empty");
            return;
        }
        // The protocol every member supports that most members prefer.
        let candidates: Vec<String> = {
            let first = g.members.values().next().unwrap();
            first
                .protocols
                .iter()
                .map(|(p, _)| p.clone())
                .filter(|p| g.members.values().all(|m| m.protocols.iter().any(|(q, _)| q == p)))
                .collect()
        };
        let mut votes: BTreeMap<&str, usize> = BTreeMap::new();
        for m in g.members.values() {
            if let Some((p, _)) = m.protocols.iter().find(|(p, _)| candidates.contains(p)) {
                *votes.entry(p.as_str()).or_default() += 1;
            }
        }
        let protocol = candidates
            .iter()
            .max_by_key(|p| (votes.get(p.as_str()).copied().unwrap_or(0), std::cmp::Reverse(candidates.iter().position(|c| c == *p))))
            .cloned();
        g.protocol = protocol;
        if g.leader.as_ref().is_none_or(|l| !g.members.contains_key(l)) {
            g.leader = g.members.values().min_by_key(|m| m.order).map(|m| m.id.clone());
        }
        g.state = State::CompletingRebalance;
        let timeout = g.members.values().map(|m| m.rebalance_timeout).max().unwrap_or(Duration::from_secs(30));
        g.sync_deadline = Some(now + timeout);
        let ids: Vec<String> = g.members.keys().cloned().collect();
        for id in ids {
            let result = g.current_join(&id);
            let m = g.members.get_mut(&id).unwrap();
            m.last_seen = now;
            if let Some(w) = m.join_waiter.take() {
                let _ = w.send(result);
            }
        }
        let _ = b.store.save_group(stream, group_id, g.protocol_type.as_deref().unwrap_or(""), g.generation, now_secs());
        debug!(stream, group = group_id, generation = g.generation, members = g.members.len(), "rebalance completed");
    }

    pub async fn sync(&self, stream: &str, req: SyncRequest) -> SyncResult {
        let rx = match self.sync_now(stream, req) {
            Ok(r) => return r,
            Err(rx) => rx,
        };
        rx.await.unwrap_or_else(|_| SyncResult::error(ResponseError::RebalanceInProgress))
    }

    fn sync_now(&self, stream: &str, req: SyncRequest) -> Result<SyncResult, oneshot::Receiver<SyncResult>> {
        let Some(group) = self.get(stream, &req.group_id) else {
            return Ok(SyncResult::error(ResponseError::UnknownMemberId));
        };
        let mut g = group.lock().unwrap();
        if let Some(e) = check_member(&g, &req.member_id, req.instance_id.as_deref(), req.generation) {
            return Ok(SyncResult::error(e));
        }
        if req.protocol_type.as_ref().is_some_and(|p| Some(p) != g.protocol_type.as_ref())
            || req.protocol_name.as_ref().is_some_and(|p| Some(p) != g.protocol.as_ref())
        {
            return Ok(SyncResult::error(ResponseError::InconsistentGroupProtocol));
        }
        let ok = |g: &Group, a: Bytes| SyncResult { error: 0, protocol_type: g.protocol_type.clone(), protocol_name: g.protocol.clone(), assignment: a };
        g.members.get_mut(&req.member_id).unwrap().last_seen = Instant::now();
        match g.state {
            State::Empty | State::Dead => Ok(SyncResult::error(ResponseError::UnknownMemberId)),
            State::PreparingRebalance => Ok(SyncResult::error(ResponseError::RebalanceInProgress)),
            State::Stable => {
                let a = g.members[&req.member_id].assignment.clone();
                Ok(ok(&g, a))
            }
            State::CompletingRebalance => {
                let (tx, rx) = oneshot::channel();
                g.members.get_mut(&req.member_id).unwrap().sync_waiter = Some(tx);
                if g.leader.as_deref() == Some(req.member_id.as_str()) {
                    let assignments: HashMap<String, Bytes> = req.assignments.into_iter().collect();
                    g.state = State::Stable;
                    g.sync_deadline = None;
                    let ids: Vec<String> = g.members.keys().cloned().collect();
                    for id in ids {
                        let a = assignments.get(&id).cloned().unwrap_or_default();
                        let result = ok(&g, a.clone());
                        let m = g.members.get_mut(&id).unwrap();
                        m.assignment = a;
                        if let Some(w) = m.sync_waiter.take() {
                            let _ = w.send(result);
                        }
                    }
                }
                Err(rx)
            }
        }
    }

    pub fn heartbeat(&self, stream: &str, group_id: &str, member_id: &str, instance_id: Option<&str>, generation: i32) -> i16 {
        let Some(group) = self.get(stream, group_id) else {
            return ResponseError::UnknownMemberId.code();
        };
        let mut g = group.lock().unwrap();
        if let Some(e) = check_member(&g, member_id, instance_id, generation) {
            return e.code();
        }
        g.members.get_mut(member_id).unwrap().last_seen = Instant::now();
        match g.state {
            State::PreparingRebalance => ResponseError::RebalanceInProgress.code(),
            State::CompletingRebalance | State::Stable => 0,
            State::Empty | State::Dead => ResponseError::UnknownMemberId.code(),
        }
    }

    /// Remove members (`(member id, instance id)`); returns a per-member error code.
    pub fn leave(&self, b: &Broker, stream: &str, group_id: &str, members: &[(String, Option<String>)]) -> Result<Vec<i16>, ResponseError> {
        let Some(group) = self.get(stream, group_id) else {
            return Err(ResponseError::UnknownMemberId);
        };
        let mut g = group.lock().unwrap();
        let mut out = Vec::with_capacity(members.len());
        let mut removed = false;
        for (member, instance) in members {
            let id = match (member.as_str(), instance) {
                ("", Some(i)) => g.statics.get(i).cloned(),
                (m, Some(i)) if g.statics.get(i).is_some_and(|x| x != m) => {
                    out.push(ResponseError::FencedInstanceId.code());
                    continue;
                }
                (m, _) => Some(m.to_string()),
            };
            match id {
                Some(id) if g.members.contains_key(&id) => {
                    g.remove_member(&id, ResponseError::UnknownMemberId);
                    removed = true;
                    out.push(0);
                }
                Some(id) if g.pending.remove(&id).is_some() => out.push(0),
                _ => out.push(ResponseError::UnknownMemberId.code()),
            }
        }
        if removed {
            self.member_gone(b, stream, group_id, &mut g);
        }
        Ok(out)
    }

    fn member_gone(&self, b: &Broker, stream: &str, group_id: &str, g: &mut Group) {
        match g.state {
            State::Stable | State::CompletingRebalance => {
                self.prepare_rebalance(b, g);
                self.maybe_complete_join(b, stream, group_id, g, false);
            }
            State::PreparingRebalance => self.maybe_complete_join(b, stream, group_id, g, false),
            _ => {}
        }
    }

    /// Drive timers: session expiry, rebalance and sync timeouts, the initial join delay.
    fn tick(&self, b: &Broker) {
        let groups: Vec<(Key, Arc<Mutex<Group>>)> = self.groups.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let now = Instant::now();
        for ((stream, group_id), group) in groups {
            let mut g = group.lock().unwrap();
            g.pending.retain(|_, deadline| *deadline > now);
            let expired: Vec<String> = g
                .members
                .values()
                .filter(|m| !m.awaiting() && now.duration_since(m.last_seen) > m.session_timeout)
                .map(|m| m.id.clone())
                .collect();
            if !expired.is_empty() {
                for id in &expired {
                    debug!(%stream, group = %group_id, member = %id, "session expired");
                    g.remove_member(id, ResponseError::UnknownMemberId);
                }
                self.member_gone(b, &stream, &group_id, &mut g);
            }
            match g.state {
                State::PreparingRebalance => {
                    if g.rebalance_deadline.is_some_and(|d| now >= d) {
                        self.maybe_complete_join(b, &stream, &group_id, &mut g, true);
                    } else {
                        self.maybe_complete_join(b, &stream, &group_id, &mut g, false);
                    }
                }
                State::CompletingRebalance if g.sync_deadline.is_some_and(|d| now >= d) => {
                    // The leader never sent assignments: drop members that didn't sync and retry.
                    let unsynced: Vec<String> = g.members.values().filter(|m| m.sync_waiter.is_none()).map(|m| m.id.clone()).collect();
                    for id in unsynced {
                        g.remove_member(&id, ResponseError::UnknownMemberId);
                    }
                    self.prepare_rebalance(b, &mut g);
                    self.maybe_complete_join(b, &stream, &group_id, &mut g, false);
                }
                _ => {}
            }
        }
    }

    // ── offsets ──

    /// Validate a commit against group membership (Kafka's `doCommitOffsets` rules).
    pub fn check_commit(&self, b: &Broker, stream: &str, group_id: &str, member_id: &str, instance_id: Option<&str>, generation: i32) -> Result<(), ResponseError> {
        if group_id.is_empty() {
            return Err(ResponseError::InvalidGroupId);
        }
        let group = match self.get(stream, group_id) {
            Some(g) => g,
            None if generation < 0 => {
                self.get_or_create(b, stream, group_id);
                return Ok(());
            }
            None => return Err(ResponseError::IllegalGeneration),
        };
        let mut g = group.lock().unwrap();
        match g.state {
            State::Dead => Err(ResponseError::CoordinatorNotAvailable),
            State::Empty if generation < 0 => Ok(()),
            State::CompletingRebalance => Err(ResponseError::RebalanceInProgress),
            _ => {
                if let Some(e) = check_member(&g, member_id, instance_id, generation) {
                    return Err(e);
                }
                g.members.get_mut(member_id).unwrap().last_seen = Instant::now();
                Ok(())
            }
        }
    }

    pub fn stage_txn_offsets(&self, stream: &str, group_id: &str, producer_id: i64, rows: Vec<OffsetRow>) {
        let mut p = self.pending_txn.lock().unwrap();
        let entry = p.entry(key(stream, group_id)).or_default().entry(producer_id).or_default();
        for r in rows {
            entry.retain(|x| !(x.topic == r.topic && x.partition == r.partition));
            entry.push(r);
        }
    }

    /// Apply (commit) or drop (abort) a producer's staged offsets for a group.
    pub fn end_txn_offsets(&self, b: &Broker, stream: &str, group_id: &str, producer_id: i64, commit: bool) -> anyhow::Result<()> {
        let rows = {
            let mut p = self.pending_txn.lock().unwrap();
            let Some(by_pid) = p.get_mut(&key(stream, group_id)) else { return Ok(()) };
            let rows = by_pid.remove(&producer_id);
            if by_pid.is_empty() {
                p.remove(&key(stream, group_id));
            }
            rows
        };
        if commit && let Some(rows) = rows {
            self.get_or_create(b, stream, group_id);
            b.store.commit_offsets(stream, group_id, &rows)?;
        }
        Ok(())
    }

    /// Partitions with staged transactional offsets (for `require_stable` fetches).
    pub fn unstable(&self, stream: &str, group_id: &str) -> Vec<(String, i32)> {
        self.pending_txn
            .lock()
            .unwrap()
            .get(&key(stream, group_id))
            .map(|m| m.values().flatten().map(|r| (r.topic.clone(), r.partition)).collect())
            .unwrap_or_default()
    }

    // ── admin ──

    pub fn describe(&self, b: &Broker, stream: &str, group_id: &str) -> Option<GroupInfo> {
        let Some(group) = self.get(stream, group_id) else {
            // A group that only has committed offsets.
            return b.store.offset_groups(stream).ok()?.contains(&group_id.to_string()).then(|| GroupInfo {
                group_id: group_id.into(),
                state: State::Empty.name(),
                protocol_type: b.store.group(stream, group_id).ok().flatten().map(|(p, _)| p).unwrap_or_else(|| "consumer".into()),
                protocol: String::new(),
                generation: 0,
                leader: None,
                members: vec![],
            });
        };
        let g = group.lock().unwrap();
        let protocol = g.protocol.clone().unwrap_or_default();
        let consumer = g.protocol_type.as_deref() == Some("consumer");
        let mut members: Vec<&Member> = g.members.values().collect();
        members.sort_by_key(|m| m.order);
        let members = members
            .into_iter()
            .map(|m| {
                let metadata = m.metadata(&protocol);
                MemberInfo {
                    topics: if consumer { crate::handlers::groups::subscription_topics(&metadata) } else { vec![] },
                    assigned: if consumer { crate::handlers::groups::assignment_partitions(&m.assignment) } else { vec![] },
                    member_id: m.id.clone(),
                    instance_id: m.instance_id.clone(),
                    client_id: m.client_id.clone(),
                    client_host: m.client_host.clone(),
                    metadata,
                    assignment: m.assignment.clone(),
                }
            })
            .collect();
        Some(GroupInfo {
            group_id: group_id.into(),
            state: g.state.name(),
            protocol_type: g.protocol_type.clone().unwrap_or_default(),
            protocol,
            generation: g.generation,
            leader: g.leader.clone(),
            members,
        })
    }

    /// `(group id, protocol type, state)` for every group of a stream, including offset-only ones.
    pub fn list(&self, b: &Broker, stream: &str) -> Vec<(String, String, &'static str)> {
        let mut out: BTreeMap<String, (String, &'static str)> = BTreeMap::new();
        for g in b.store.offset_groups(stream).unwrap_or_default() {
            let pt = b.store.group(stream, &g).ok().flatten().map(|(p, _)| p).filter(|p| !p.is_empty()).unwrap_or_else(|| "consumer".into());
            out.insert(g, (pt, State::Empty.name()));
        }
        for ((s, id), group) in self.groups.lock().unwrap().iter() {
            if s != stream {
                continue;
            }
            let g = group.lock().unwrap();
            if g.state == State::Dead {
                continue;
            }
            let pt = g.protocol_type.clone().filter(|p| !p.is_empty()).unwrap_or_else(|| "consumer".into());
            out.insert(id.clone(), (pt, g.state.name()));
        }
        out.into_iter().map(|(id, (pt, st))| (id, pt, st)).collect()
    }

    pub fn delete(&self, b: &Broker, stream: &str, group_id: &str) -> Result<(), ResponseError> {
        let known = b.store.offset_groups(stream).map_err(|_| ResponseError::UnknownServerError)?.iter().any(|g| g == group_id);
        let mut map = self.groups.lock().unwrap();
        if let Some(group) = map.get(&key(stream, group_id)) {
            let g = group.lock().unwrap();
            if !g.members.is_empty() || g.state != State::Empty {
                return Err(ResponseError::NonEmptyGroup);
            }
        } else if !known {
            return Err(ResponseError::GroupIdNotFound);
        }
        map.remove(&key(stream, group_id));
        drop(map);
        b.store.delete_offsets(stream, group_id, None).map_err(|_| ResponseError::UnknownServerError)?;
        Ok(())
    }

    /// Topics the active members are subscribed to (OffsetDelete refuses those).
    pub fn subscribed_topics(&self, stream: &str, group_id: &str) -> Option<Vec<String>> {
        let group = self.get(stream, group_id)?;
        let g = group.lock().unwrap();
        if g.protocol_type.as_deref() != Some("consumer") {
            return Some(vec![]);
        }
        let protocol = g.protocol.clone().unwrap_or_default();
        let mut topics: Vec<String> =
            g.members.values().flat_map(|m| crate::handlers::groups::subscription_topics(&m.metadata(&protocol))).collect();
        topics.sort();
        topics.dedup();
        Some(topics)
    }

    pub fn is_active(&self, stream: &str, group_id: &str) -> bool {
        self.get(stream, group_id).is_some_and(|g| !g.lock().unwrap().members.is_empty())
    }

    /// Expire offsets of empty groups.
    fn expire_offsets(&self, b: &Broker) {
        let cutoff = now() - b.config.groups.offsets_retention_ms / 1000;
        for s in b.streams() {
            let idle: Vec<String> = b.store.offset_groups(&s.id).unwrap_or_default().into_iter().filter(|g| !self.is_active(&s.id, g)).collect();
            if let Ok(n) = b.store.expire_offsets(&s.id, &idle, cutoff)
                && n > 0
            {
                debug!(stream = %s.id, offsets = n, "expired committed offsets");
            }
        }
    }
}

fn check_member(g: &Group, member_id: &str, instance_id: Option<&str>, generation: i32) -> Option<ResponseError> {
    if let Some(i) = instance_id
        && g.statics.get(i).is_some_and(|m| m != member_id)
    {
        return Some(ResponseError::FencedInstanceId);
    }
    if !g.members.contains_key(member_id) {
        return Some(ResponseError::UnknownMemberId);
    }
    if generation != g.generation {
        return Some(ResponseError::IllegalGeneration);
    }
    None
}

fn now_secs() -> i64 {
    now_ms() / 1000
}

/// Timer loop for all groups.
pub async fn run(b: Arc<Broker>) {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut n = 0u64;
    loop {
        tick.tick().await;
        b.groups.tick(&b);
        n += 1;
        if n.is_multiple_of(6000) {
            b.groups.expire_offsets(&b);
        }
    }
}
