//! Produce/consume throughput with librdkafka against a running broker, without Docker in the way.
//!
//! ```sh
//! cargo run --release -p rolld &
//! cargo run --release -p stream-broker --example bench -- 127.0.0.1:9092 5000000 200
//! ```

use rdkafka::ClientConfig;
use rdkafka::admin::{AdminClient, AdminOptions, NewTopic, TopicReplication};
use rdkafka::client::DefaultClientContext;
use rdkafka::consumer::{BaseConsumer, Consumer};
use rdkafka::producer::{BaseRecord, DeliveryResult, Producer, ProducerContext, ThreadedProducer};
use rdkafka::{ClientContext, Offset, TopicPartitionList};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const PARTITIONS: i32 = 6;

#[derive(Default)]
struct Counts {
    ok: AtomicU64,
    failed: AtomicU64,
    /// Sum of acknowledgement latencies in µs.
    latency_us: AtomicU64,
}

struct Ctx(Arc<Counts>);

impl ClientContext for Ctx {}

impl ProducerContext for Ctx {
    type DeliveryOpaque = Box<Instant>;
    fn delivery(&self, r: &DeliveryResult<'_>, sent: Box<Instant>) {
        match r {
            Ok(_) => {
                self.0.ok.fetch_add(1, Ordering::Relaxed);
                self.0.latency_us.fetch_add(sent.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            Err(_) => {
                self.0.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn mb(bytes: u64, d: Duration) -> f64 {
    bytes as f64 / 1e6 / d.as_secs_f64()
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:9092".into());
    let n: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(5_000_000);
    let size: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(200);
    let topic = format!("bench-{}", std::process::id());

    let admin: AdminClient<DefaultClientContext> = ClientConfig::new().set("bootstrap.servers", &addr).create().unwrap();
    admin.create_topics(&[NewTopic::new(&topic, PARTITIONS, TopicReplication::Fixed(1))], &AdminOptions::new()).await.unwrap();

    let counts = Arc::new(Counts::default());
    let producer: ThreadedProducer<Ctx> = ClientConfig::new()
        .set("bootstrap.servers", &addr)
        .set("acks", "all")
        .set("linger.ms", "5")
        .set("batch.size", "1000000")
        .set("queue.buffering.max.kbytes", "1048576")
        .set("queue.buffering.max.messages", "2000000")
        .create_with_context(Ctx(counts.clone()))
        .unwrap();
    let payload = vec![b'x'; size];
    let start = Instant::now();
    for i in 0..n {
        let p = (i % PARTITIONS as u64) as i32;
        let mut rec = BaseRecord::<(), [u8], Box<Instant>>::with_opaque_to(&topic, Box::new(Instant::now())).payload(&payload[..]).partition(p);
        loop {
            match producer.send(rec) {
                Ok(()) => break,
                Err((_, r)) => {
                    rec = r;
                    std::thread::sleep(Duration::from_micros(200));
                }
            }
        }
    }
    producer.flush(Duration::from_secs(120)).unwrap();
    let took = start.elapsed();
    let ok = counts.ok.load(Ordering::Relaxed);
    println!(
        "produce  {n} × {size} B, acks=all: {:.0} rec/s, {:.1} MB/s, avg ack latency {:.1} ms, {} failed",
        ok as f64 / took.as_secs_f64(),
        mb(ok * size as u64, took),
        counts.latency_us.load(Ordering::Relaxed) as f64 / ok.max(1) as f64 / 1000.0,
        counts.failed.load(Ordering::Relaxed)
    );

    let consumer: BaseConsumer = ClientConfig::new()
        .set("bootstrap.servers", &addr)
        .set("group.id", "bench")
        .set("enable.auto.commit", "false")
        .set("fetch.max.bytes", "104857600")
        .set("max.partition.fetch.bytes", "16777216")
        .set("queued.min.messages", "1000000")
        .create()
        .unwrap();
    let mut tpl = TopicPartitionList::new();
    for p in 0..PARTITIONS {
        tpl.add_partition_offset(&topic, p, Offset::Beginning).unwrap();
    }
    consumer.assign(&tpl).unwrap();
    let start = Instant::now();
    let (mut got, mut bytes) = (0u64, 0u64);
    while got < ok && start.elapsed() < Duration::from_secs(120) {
        if let Some(Ok(m)) = consumer.poll(Duration::from_millis(100)) {
            got += 1;
            bytes += m.payload_len() as u64;
        }
    }
    let took = start.elapsed();
    println!("consume  {got} records: {:.0} rec/s, {:.1} MB/s", got as f64 / took.as_secs_f64(), mb(bytes, took));
    let _ = admin.delete_topics(&[&topic], &AdminOptions::new()).await;
}
