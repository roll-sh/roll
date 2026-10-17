//! Transaction coordinator: transactional ids → producer id/epoch, the partitions and groups in
//! the open transaction, and the two-phase end (persist `Prepare*`, write markers, persist
//! `Complete*`) so a crash between the phases is finished on restart.

use crate::broker::Broker;
use crate::ids::now_ms;
use crate::store::TxnRow;
use kafka_protocol::ResponseError;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, info, warn};

pub const EMPTY: &str = "Empty";
pub const ONGOING: &str = "Ongoing";
pub const PREPARE_COMMIT: &str = "PrepareCommit";
pub const PREPARE_ABORT: &str = "PrepareAbort";
pub const COMPLETE_COMMIT: &str = "CompleteCommit";
pub const COMPLETE_ABORT: &str = "CompleteAbort";

type Key = (String, String);
type Entry = Arc<tokio::sync::Mutex<TxnRow>>;

#[derive(Default)]
pub struct Txns {
    map: Mutex<HashMap<Key, Entry>>,
    /// `(stream, producer id) → transactional id`, to check transactional produce requests.
    by_pid: Mutex<HashMap<(String, i64), String>>,
}

pub struct InitResult {
    pub error: i16,
    pub producer_id: i64,
    pub epoch: i16,
}

impl Txns {
    fn entry(&self, stream: &str, txn_id: &str) -> Option<Entry> {
        self.map.lock().unwrap().get(&(stream.to_string(), txn_id.to_string())).cloned()
    }

    pub fn drop_stream(&self, stream: &str) {
        self.map.lock().unwrap().retain(|(s, _), _| s != stream);
        self.by_pid.lock().unwrap().retain(|(s, _), _| s != stream);
    }

    /// Load persisted transactions and finish any that were mid-commit/abort.
    pub async fn recover(&self, b: &Broker) -> anyhow::Result<()> {
        let rows = b.store.txns()?;
        let mut unfinished = Vec::new();
        for row in rows {
            let key = (row.stream_id.clone(), row.txn_id.clone());
            self.by_pid.lock().unwrap().insert((row.stream_id.clone(), row.producer_id), row.txn_id.clone());
            if row.state == PREPARE_COMMIT || row.state == PREPARE_ABORT {
                unfinished.push(key.clone());
            }
            self.map.lock().unwrap().insert(key, Arc::new(tokio::sync::Mutex::new(row)));
        }
        for (stream, id) in unfinished {
            if let Some(e) = self.entry(&stream, &id) {
                let mut t = e.lock().await;
                let commit = t.state == PREPARE_COMMIT;
                info!(%stream, txn = %id, commit, "finishing transaction interrupted by restart");
                if let Err(e) = self.complete(b, &mut t, commit).await {
                    warn!(%stream, txn = %id, error = %e, "finishing transaction");
                }
            }
        }
        Ok(())
    }

    /// InitProducerId. Without a transactional id this just allocates (or bumps) an idempotent
    /// producer id.
    pub async fn init(&self, b: &Broker, stream: &str, txn_id: Option<&str>, timeout_ms: i32, pid: i64, epoch: i16) -> InitResult {
        let err = |e: ResponseError| InitResult { error: e.code(), producer_id: -1, epoch: -1 };
        let Some(txn_id) = txn_id.filter(|t| !t.is_empty()) else {
            // KIP-360: an idempotent producer may ask to bump its epoch after a sequence error.
            if pid >= 0 && (0..i16::MAX - 1).contains(&epoch) {
                return InitResult { error: 0, producer_id: pid, epoch: epoch + 1 };
            }
            return match b.next_producer_id() {
                Ok(id) => InitResult { error: 0, producer_id: id, epoch: 0 },
                Err(_) => err(ResponseError::UnknownServerError),
            };
        };
        if timeout_ms <= 0 || timeout_ms > b.config.txn.max_timeout_ms {
            return err(ResponseError::InvalidTransactionTimeout);
        }
        let key = (stream.to_string(), txn_id.to_string());
        let entry = {
            let mut m = self.map.lock().unwrap();
            match m.get(&key) {
                Some(e) => e.clone(),
                None => {
                    let Ok(id) = b.next_producer_id() else { return err(ResponseError::UnknownServerError) };
                    let row = TxnRow {
                        stream_id: stream.into(),
                        txn_id: txn_id.into(),
                        producer_id: id,
                        epoch: -1,
                        state: EMPTY.into(),
                        timeout_ms,
                        partitions: BTreeMap::new(),
                        groups: vec![],
                        started_at: 0,
                        updated_at: now_ms(),
                    };
                    let e = Arc::new(tokio::sync::Mutex::new(row));
                    m.insert(key, e.clone());
                    e
                }
            }
        };
        let mut t = entry.lock().await;
        if pid >= 0 && (pid != t.producer_id || epoch != t.epoch) && t.epoch >= 0 {
            // KIP-360 retry with a stale identity.
            return err(ResponseError::ProducerFenced);
        }
        match t.state.as_str() {
            PREPARE_COMMIT | PREPARE_ABORT => {
                let commit = t.state == PREPARE_COMMIT;
                if let Err(e) = self.complete(b, &mut t, commit).await {
                    warn!(error = %e, "completing transaction");
                    return err(ResponseError::ConcurrentTransactions);
                }
            }
            ONGOING => {
                // A new instance fences the old one: abort its transaction with a bumped epoch.
                self.bump(b, &mut t);
                t.state = PREPARE_ABORT.into();
                if let Err(e) = self.persist(b, &t) {
                    warn!(error = %e, "persisting transaction");
                }
                if let Err(e) = self.complete(b, &mut t, false).await {
                    warn!(error = %e, "aborting fenced transaction");
                    return err(ResponseError::ConcurrentTransactions);
                }
                t.state = EMPTY.into();
                t.timeout_ms = timeout_ms;
                let _ = self.persist(b, &t);
                return InitResult { error: 0, producer_id: t.producer_id, epoch: t.epoch };
            }
            _ => {}
        }
        self.bump(b, &mut t);
        t.state = EMPTY.into();
        t.timeout_ms = timeout_ms;
        t.partitions.clear();
        t.groups.clear();
        if self.persist(b, &t).is_err() {
            return err(ResponseError::UnknownServerError);
        }
        InitResult { error: 0, producer_id: t.producer_id, epoch: t.epoch }
    }

    fn bump(&self, b: &Broker, t: &mut TxnRow) {
        if t.epoch >= i16::MAX - 1 {
            if let Ok(id) = b.next_producer_id() {
                self.by_pid.lock().unwrap().remove(&(t.stream_id.clone(), t.producer_id));
                t.producer_id = id;
                t.epoch = 0;
            }
        } else {
            t.epoch += 1;
        }
        self.by_pid.lock().unwrap().insert((t.stream_id.clone(), t.producer_id), t.txn_id.clone());
    }

    fn persist(&self, b: &Broker, t: &TxnRow) -> anyhow::Result<()> {
        b.store.save_txn(t)
    }

    async fn locked(&self, stream: &str, txn_id: &str, pid: i64, epoch: i16) -> Result<tokio::sync::OwnedMutexGuard<TxnRow>, ResponseError> {
        let e = self.entry(stream, txn_id).ok_or(ResponseError::InvalidProducerIdMapping)?;
        let t = e.lock_owned().await;
        if t.producer_id != pid {
            return Err(ResponseError::InvalidProducerIdMapping);
        }
        if epoch < t.epoch {
            return Err(ResponseError::ProducerFenced);
        }
        if epoch > t.epoch {
            return Err(ResponseError::InvalidProducerEpoch);
        }
        Ok(t)
    }

    /// AddPartitionsToTxn for one transaction. `partitions` must already be validated as existing.
    pub async fn add_partitions(&self, b: &Broker, stream: &str, txn_id: &str, pid: i64, epoch: i16, partitions: &[(String, i32)]) -> Result<(), ResponseError> {
        let mut t = self.locked(stream, txn_id, pid, epoch).await?;
        if t.state == PREPARE_COMMIT || t.state == PREPARE_ABORT {
            return Err(ResponseError::ConcurrentTransactions);
        }
        let mut changed = t.state != ONGOING;
        if t.state != ONGOING {
            t.state = ONGOING.into();
            t.started_at = now_ms();
            t.partitions.clear();
            t.groups.clear();
        }
        for (topic, p) in partitions {
            let ps = t.partitions.entry(topic.clone()).or_default();
            if !ps.contains(p) {
                ps.push(*p);
                ps.sort();
                changed = true;
            }
        }
        if changed {
            t.updated_at = now_ms();
            self.persist(b, &t).map_err(|_| ResponseError::UnknownServerError)?;
        }
        Ok(())
    }

    /// Check whether a partition is part of a transaction (verification of transactional produce,
    /// as in KIP-890), returning the error to report otherwise.
    pub async fn verify(&self, stream: &str, pid: i64, epoch: i16, topic: &str, partition: i32) -> Result<(), ResponseError> {
        let Some(txn_id) = self.by_pid.lock().unwrap().get(&(stream.to_string(), pid)).cloned() else {
            return Err(ResponseError::InvalidProducerIdMapping);
        };
        let t = self.locked(stream, &txn_id, pid, epoch).await?;
        if t.state == ONGOING && t.partitions.get(topic).is_some_and(|ps| ps.contains(&partition)) {
            Ok(())
        } else {
            Err(ResponseError::InvalidTxnState)
        }
    }

    pub async fn add_offsets(&self, b: &Broker, stream: &str, txn_id: &str, pid: i64, epoch: i16, group: &str) -> Result<(), ResponseError> {
        let mut t = self.locked(stream, txn_id, pid, epoch).await?;
        if t.state == PREPARE_COMMIT || t.state == PREPARE_ABORT {
            return Err(ResponseError::ConcurrentTransactions);
        }
        if t.state != ONGOING {
            t.state = ONGOING.into();
            t.started_at = now_ms();
            t.partitions.clear();
            t.groups.clear();
        }
        if !t.groups.iter().any(|g| g == group) {
            t.groups.push(group.into());
        }
        t.updated_at = now_ms();
        self.persist(b, &t).map_err(|_| ResponseError::UnknownServerError)
    }

    /// TxnOffsetCommit is only valid for a group added to the producer's open transaction.
    pub async fn check_offsets(&self, stream: &str, txn_id: &str, pid: i64, epoch: i16, group: &str) -> Result<(), ResponseError> {
        let t = self.locked(stream, txn_id, pid, epoch).await?;
        if t.state != ONGOING || !t.groups.iter().any(|g| g == group) {
            return Err(ResponseError::InvalidTxnState);
        }
        Ok(())
    }

    pub async fn end(&self, b: &Broker, stream: &str, txn_id: &str, pid: i64, epoch: i16, commit: bool) -> Result<(), ResponseError> {
        let mut t = self.locked(stream, txn_id, pid, epoch).await?;
        match (t.state.as_str(), commit) {
            (ONGOING, _) => {}
            (COMPLETE_COMMIT, true) | (COMPLETE_ABORT, false) => return Ok(()),
            (PREPARE_COMMIT, true) | (PREPARE_ABORT, false) => {}
            (PREPARE_COMMIT | PREPARE_ABORT, _) => return Err(ResponseError::ConcurrentTransactions),
            _ => return Err(ResponseError::InvalidTxnState),
        }
        t.state = if commit { PREPARE_COMMIT } else { PREPARE_ABORT }.into();
        t.updated_at = now_ms();
        self.persist(b, &t).map_err(|_| ResponseError::UnknownServerError)?;
        self.complete(b, &mut t, commit).await.map_err(|e| {
            warn!(error = %e, "writing transaction markers");
            ResponseError::UnknownServerError
        })
    }

    /// Phase two: markers on every partition, then the group offsets, then `Complete*`.
    async fn complete(&self, b: &Broker, t: &mut TxnRow, commit: bool) -> anyhow::Result<()> {
        let stream = b.stream(&t.stream_id).ok_or_else(|| anyhow::anyhow!("stream is gone"))?;
        let mut waits = Vec::new();
        for (topic_name, parts) in &t.partitions {
            let Some(topic) = stream.topic(topic_name) else { continue };
            for &i in parts {
                let Some(p) = topic.partition(i) else { continue };
                let log = b.log(&topic, &p).map_err(|e| anyhow::anyhow!(e.message()))?;
                let (offset, aborted) = log
                    .append_marker(t.producer_id, t.epoch, commit, 0, now_ms())
                    .map_err(|e| anyhow::anyhow!("marker on {topic_name}-{i}: {e}"))?;
                if let Some(a) = aborted {
                    b.store.insert_aborted(&topic.id, i, &a)?;
                }
                b.request_flush(&p, &log);
                waits.push((log.subscribe(), offset + 1));
            }
        }
        for g in &t.groups {
            b.groups.end_txn_offsets(b, &t.stream_id, g, t.producer_id, commit)?;
        }
        if b.config.fsync {
            for (mut rx, target) in waits {
                let _ = tokio::time::timeout(Duration::from_secs(30), rx.wait_for(|hw| *hw >= target)).await;
            }
        }
        t.state = if commit { COMPLETE_COMMIT } else { COMPLETE_ABORT }.into();
        // Markers are down; like Kafka, a completed transaction lists no partitions.
        t.partitions.clear();
        t.groups.clear();
        t.updated_at = now_ms();
        self.persist(b, t)?;
        debug!(stream = %t.stream_id, txn = %t.txn_id, commit, "transaction completed");
        Ok(())
    }

    /// Abort transactions open longer than their timeout, fencing the producer.
    async fn expire(&self, b: &Broker) {
        let entries: Vec<Entry> = self.map.lock().unwrap().values().cloned().collect();
        let now = now_ms();
        for e in entries {
            let Ok(mut t) = e.try_lock() else { continue };
            if t.state == ONGOING && now - t.started_at > t.timeout_ms as i64 {
                info!(stream = %t.stream_id, txn = %t.txn_id, "aborting timed out transaction");
                self.bump(b, &mut t);
                t.state = PREPARE_ABORT.into();
                let _ = self.persist(b, &t);
                if let Err(err) = self.complete(b, &mut t, false).await {
                    warn!(error = %err, "aborting timed out transaction");
                }
            } else if (t.state == EMPTY || t.state == COMPLETE_COMMIT || t.state == COMPLETE_ABORT)
                && now - t.updated_at > b.config.txn.id_expiration_ms
            {
                let key = (t.stream_id.clone(), t.txn_id.clone());
                self.by_pid.lock().unwrap().remove(&(t.stream_id.clone(), t.producer_id));
                let _ = b.store.delete_txn(&t.stream_id, &t.txn_id);
                drop(t);
                self.map.lock().unwrap().remove(&key);
            }
        }
    }

    pub fn list(&self, stream: &str) -> Vec<TxnRow> {
        let entries: Vec<Entry> =
            self.map.lock().unwrap().iter().filter(|((s, _), _)| s == stream).map(|(_, e)| e.clone()).collect();
        let mut out: Vec<TxnRow> = entries.iter().filter_map(|e| e.try_lock().ok().map(|t| t.clone())).collect();
        out.sort_by(|a, b| a.txn_id.cmp(&b.txn_id));
        out
    }

    pub fn describe(&self, stream: &str, txn_id: &str) -> Option<TxnRow> {
        let e = self.entry(stream, txn_id)?;
        e.try_lock().ok().map(|t| t.clone())
    }
}

pub async fn run(b: Arc<Broker>) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        b.txns.expire(&b).await;
    }
}
