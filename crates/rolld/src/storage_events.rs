//! roll storage's event notifications into roll queues and roll streams, in process: the
//! [`s3_server::EventSink`] behind `arn:roll:queues:<vhost>:<queue>` and
//! `arn:roll:streams:<stream>:<topic>` destinations.
//!
//! - Queues: the event JSON is published straight to the queue (default exchange, persistent,
//!   `content-type: application/json`, `type` the event name, `message-id` the delivery id), and
//!   counts as delivered once the queue confirms it. A vhost scaled to zero wakes up.
//! - Streams: one record per event, keyed by the object key (so a key's events stay in one
//!   partition, in order), headers `s3-event`, `s3-bucket` and `delivery-id`, delivered once
//!   durable (`acks=all`).
//!
//! A destination that doesn't exist (yet) is an error: storage retries, then drops the event
//! after its retention window.

use bytes::Bytes;
use futures::future::BoxFuture;
use mq_core::queue::ConfirmTicket;
use mq_core::{Message, Properties};
use s3_server::{Delivery, EventSink, RollDestination};
use std::sync::Arc;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

pub struct RollSink {
    pub queues: Option<Arc<mq_core::Broker>>,
    pub streams: Arc<stream_broker::broker::Broker>,
}

impl RollSink {
    async fn vhost(&self, name: &str) -> Result<Arc<mq_core::VHost>, String> {
        let b = self.queues.as_ref().ok_or("roll queues are disabled on this deployment")?;
        if !b.vhost_exists(name) {
            return Err(format!("vhost {name} doesn't exist"));
        }
        b.open_vhost(name).await.ok_or_else(|| format!("vhost {name} is unavailable"))
    }

    fn topic(&self, stream: &str, topic: &str) -> Result<(Arc<stream_broker::broker::Stream>, Arc<stream_broker::broker::Topic>), String> {
        let s = self.streams.find_stream(stream).ok_or_else(|| format!("stream {stream} doesn't exist"))?;
        let t = s.topic(topic).ok_or_else(|| format!("topic {topic} doesn't exist in stream {stream}"))?;
        Ok((s, t))
    }

    async fn publish(&self, vhost: &str, queue: &str, d: &Delivery) -> Result<(), String> {
        let vh = self.vhost(vhost).await?;
        if vh.queue(queue).is_none() {
            return Err(format!("queue {queue} doesn't exist in vhost {vhost}"));
        }
        let props = Properties {
            content_type: Some("application/json".into()),
            delivery_mode: Some(2),
            message_id: Some(d.id.clone()),
            kind: Some(d.event_name.clone()),
            app_id: Some("roll-storage".into()),
            ..Default::default()
        };
        let msg = Message::new("", queue, props, d.body.clone());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let ticket = ConfirmTicket::new(move |ok| {
            let _ = tx.send(ok);
        });
        vh.publish_to_queue(queue, Arc::new(msg), Some(ticket)).await.map_err(|e| e.to_string())?;
        match tokio::time::timeout(TIMEOUT, rx).await {
            Ok(Ok(true)) => Ok(()),
            Ok(_) => Err(format!("queue {queue} refused the message")),
            Err(_) => Err(format!("queue {queue} didn't confirm in time")),
        }
    }
}

impl EventSink for RollSink {
    fn check<'a>(&'a self, dest: &'a RollDestination) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            match dest {
                RollDestination::Queue { vhost, queue } => {
                    let vh = self.vhost(vhost).await?;
                    vh.queue(queue).map(|_| ()).ok_or_else(|| format!("queue {queue} doesn't exist in vhost {vhost}"))
                }
                RollDestination::Stream { stream, topic } => self.topic(stream, topic).map(|_| ()),
            }
        })
    }

    fn deliver<'a>(&'a self, dest: &'a RollDestination, d: &'a Delivery) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            match dest {
                RollDestination::Queue { vhost, queue } => self.publish(vhost, queue, d).await,
                RollDestination::Stream { stream, topic } => {
                    let (s, t) = self.topic(stream, topic)?;
                    let headers = vec![("s3-event".to_string(), Bytes::from(d.event_name.clone())), ("s3-bucket".into(), Bytes::from(d.bucket.clone())), ("delivery-id".into(), Bytes::from(d.id.clone()))];
                    let key = (!d.key.is_empty()).then(|| Bytes::from(d.key.clone()));
                    stream_broker::http::produce_one(&self.streams, &s, &t, key, d.body.clone(), headers, TIMEOUT).await
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mq_core::log::MemStore;
    use mq_core::meta::MemMeta;
    use mq_core::vhost::{DeclareQueue, VHostInfo};
    use mq_core::{Broker, BrokerConfig};

    /// A storage event lands in a roll queue: an in-process queues broker, the S3 server's
    /// notifier, and a write.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_roll_queue_receives_storage_events() {
        let dir = tempfile::tempdir().unwrap();
        let mq = Broker::start(BrokerConfig::default(), MemMeta::new(), MemStore::new()).await.unwrap();
        let vh = mq.create_vhost(VHostInfo { name: "shop".into(), ..Default::default() }).await.unwrap();
        vh.declare_queue(DeclareQueue { name: "storage-events".into(), durable: true, exclusive_owner: None, auto_delete: false, args: Default::default(), passive: false }).await.unwrap();
        let streams = stream_broker::broker::Broker::new(stream_broker::config::Config::local(dir.path().join("streams"))).await.unwrap();
        tokio::spawn(streams.clone().run());
        let (stream, _, _) = streams.create_stream("media", Default::default()).unwrap();
        let topic = streams.create_topic(&stream, "events", 1, -1, Default::default(), false).unwrap().unwrap();

        let store = s3_store::Store::open(s3_store::StoreConfig::new(dir.path().join("s3")).manual(), s3_store::test_backend("rolld-events").await).await.unwrap();
        let server = s3_server::Server::new(store.clone(), s3_server::Config { spool_dir: dir.path().to_path_buf(), ..Default::default() });
        server.notifier.set_sink(Arc::new(RollSink { queues: Some(mq.clone()), streams: streams.clone() }));
        store.create_bucket("uploads", Default::default()).await.unwrap();
        let mut cfg = s3_store::NotificationConfig {
            configurations: vec![s3_store::NotificationTarget {
                destination: "arn:roll:queues:shop:storage-events".into(),
                events: vec!["s3:ObjectCreated:*".into()],
                prefix: Some("a".into()),
                ..Default::default()
            }, s3_store::NotificationTarget {
                destination: "arn:roll:streams:media:events".into(),
                events: vec!["s3:ObjectCreated:*".into()],
                prefix: Some("b".into()),
                ..Default::default()
            }],
            event_bridge: false,
        };
        let bucket = store.bucket("uploads").unwrap();
        // Validation: the queue exists and takes the test event; a missing one is refused.
        cfg.configurations[0].destination = "arn:roll:queues:shop:nope".into();
        assert!(server.notifier.put_config(&bucket, cfg.clone(), false).await.is_err());
        cfg.configurations[0].destination = "arn:roll:queues:shop:storage-events".into();
        server.notifier.put_config(&bucket, cfg, false).await.unwrap();
        s3_server::put_object(&store, &bucket, "a.txt", Default::default(), s3_store::bytes_body(Bytes::from_static(b"hi")), 2, &Default::default()).await.unwrap();
        s3_server::put_object(&store, &bucket, "b.txt", Default::default(), s3_store::bytes_body(Bytes::from_static(b"hi")), 2, &Default::default()).await.unwrap();
        server.dispatch_events().await.unwrap();
        let q = vh.queue("storage-events").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while q.shared.messages() < 2 {
            assert!(std::time::Instant::now() < deadline, "the queue got {} messages", q.shared.messages());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // The stream: a test event from each validation (the refused configuration's too, as on
        // S3) and b.txt's event, durable.
        let log = streams.log(&topic, &topic.partition(0).unwrap()).unwrap();
        assert_eq!(log.high_watermark(), 3);
    }
}
