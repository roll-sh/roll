//! Copy-on-write branches of a stream (`POST /v1/streams/{s}/branches`, for roll previews).
//!
//! A branch is a new stream (its own id and keys) with the parent's topics, partitions and
//! configs, whose partitions start with the parent's records as of the branch, at the same
//! offsets, and its committed group offsets. Nothing is copied: the branch's chunk rows name the
//! parent's objects. After that the two are independent: each writes its own chunks under its
//! own prefix, and retention, compaction and deletes on either side only delete an object no
//! chunk row of any stream references (`store.rs`, "Shared chunks").
//!
//! The cut. Every partition of the parent is opened and uploaded until object storage has
//! everything visible when the branch was asked for, then each log gives a consistent cut
//! ([`stream_log::Log::branch_point`]): the end of what's uploaded and the transactions open at
//! that moment. The branch gets the chunks below the cut; those open transactions are aborted
//! in it (their markers are after the cut, and their producers never write to the branch). On
//! the fleet this runs on the parent's owner, the catalog write is fenced on its tenure, and the
//! cut is what object storage has, so the branch's high watermark is never past it.

use crate::auth;
use crate::broker::{Broker, Result, Stream, stream_exists};
use crate::error::KafkaError;
use crate::ids::{new_id, now};
use crate::store::{Credential, Cut, StreamRow};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tracing::info;

/// How long a branch waits for the parent's tail to reach object storage.
const UPLOAD_WAIT: Duration = Duration::from_secs(30);

impl Broker {
    /// Branch `parent` into a new stream `name`: `(branch, its first key, the key's secret)`.
    pub async fn branch_stream(&self, parent: &Arc<Stream>, name: &str) -> Result<(Arc<Stream>, Credential, String)> {
        let name = self.new_stream_name(name)?;
        if !self.serves(parent) {
            return Err(KafkaError::code(kafka_protocol::ResponseError::NotLeaderOrFollower));
        }
        // Everything visible now goes to object storage.
        let mut logs = vec![];
        for t in parent.topics() {
            for p in t.partitions() {
                let log = self.log(&t, &p)?;
                let visible = log.high_watermark();
                logs.push((t.id, p.index, log, visible));
            }
        }
        let deadline = Instant::now() + UPLOAD_WAIT;
        loop {
            self.upload_stream(parent).await;
            if logs.iter().all(|(_, _, log, visible)| log.uploaded_end() >= *visible) {
                break;
            }
            if Instant::now() > deadline {
                return Err(KafkaError::storage("the stream's latest records couldn't be uploaded to object storage; try again"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // No transaction between its marker and its `aborted_txns` row while the cut is taken and
        // the catalog (which copies the aborted index) is written.
        let _cutting = self.markers.write().await;
        let cuts: HashMap<(uuid::Uuid, i32), Cut> = logs
            .iter()
            .map(|(topic, index, log, _)| {
                let at = log.branch_point();
                ((*topic, *index), Cut { log_start: at.log_start, end: at.end, open_txns: at.open_txns })
            })
            .collect();

        let row = StreamRow { id: new_id("st"), name: name.into(), region: parent.row.read().unwrap().region.clone(), created_at: now(), settings: parent.settings() };
        let (cred, secret) = auth::new_credential(&row.id, "default");
        let (store, pid, r, c) = (self.store.clone(), parent.id.clone(), row.clone(), cred.clone());
        let topics = crate::fleet::blocking(move || store.branch_stream(&pid, &r, &c, &cuts, now()))
            .await
            .map_err(|e| stream_exists(e, name))?
            .ok_or_else(|| KafkaError::not_found(format!("stream {}", parent.id)))?;
        drop(_cutting);

        let stream = self.add_stream(row, topics);
        // Retention has to see its data while it has no local files (as `load` does).
        for (topic, index) in self.store.partitions_with_chunks(&stream.id).map_err(KafkaError::internal)? {
            if let Some(p) = stream.topic_by_id(&topic).and_then(|t| t.partition(index)) {
                p.remote_data.store(true, Ordering::Relaxed);
            }
        }
        info!(stream = %stream.id, parent = %parent.id, name, "stream branched");
        Ok((stream, cred, secret))
    }
}
