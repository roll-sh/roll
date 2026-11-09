package roll.qtest;

import com.rabbitmq.stream.ByteCapacity;
import com.rabbitmq.stream.ConfirmationStatus;
import com.rabbitmq.stream.Consumer;
import com.rabbitmq.stream.Environment;
import com.rabbitmq.stream.Message;
import com.rabbitmq.stream.OffsetSpecification;
import com.rabbitmq.stream.Producer;
import com.rabbitmq.stream.StreamDoesNotExistException;
import com.rabbitmq.stream.StreamStats;
import com.rabbitmq.stream.compression.Compression;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.UUID;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicInteger;

/** roll queues compatibility scenarios for RabbitMQ's stream Java client (com.rabbitmq:stream-client). */
public final class Main {
    static final String HOST = System.getenv().getOrDefault("AMQP_HOST", "localhost");
    static final int PORT = Integer.parseInt(System.getenv().getOrDefault("STREAM_PORT", "5552"));
    static final String AMQP_URL = System.getenv().getOrDefault("AMQP_URL", "amqp://guest:guest@localhost:5672/%2F");
    static final String P = "stream-" + UUID.randomUUID().toString().substring(0, 6);
    static final Map<String, Scenario> SCENARIOS = new LinkedHashMap<>();
    static Environment env;

    interface Scenario {
        String run() throws Exception;
    }

    static void check(boolean cond, String msg) {
        if (!cond) {
            throw new AssertionError(msg);
        }
    }

    static byte[] bytes(String s) {
        return s.getBytes(StandardCharsets.UTF_8);
    }

    static String str(Message m) {
        return new String(m.getBodyAsBinary(), StandardCharsets.UTF_8);
    }

    /** Send and wait for every confirm. */
    static void sendAll(Producer p, List<Message> msgs) throws InterruptedException {
        CountDownLatch done = new CountDownLatch(msgs.size());
        AtomicInteger failed = new AtomicInteger();
        for (Message m : msgs) {
            p.send(m, (ConfirmationStatus s) -> {
                if (!s.isConfirmed()) {
                    failed.incrementAndGet();
                }
                done.countDown();
            });
        }
        check(done.await(20, TimeUnit.SECONDS), "confirms outstanding " + done.getCount());
        check(failed.get() == 0, failed.get() + " not confirmed");
    }

    static List<Message> texts(Producer p, int from, int to) {
        List<Message> out = new ArrayList<>();
        for (int i = from; i < to; i++) {
            out.add(p.messageBuilder().addData(bytes("m" + i)).build());
        }
        return out;
    }

    static void publish(String stream, int from, int to) throws InterruptedException {
        Producer p = env.producerBuilder().stream(stream).build();
        sendAll(p, texts(p, from, to));
        p.close();
    }

    /** Read up to n messages from the stream with the given offset, then any extra within 300 ms. */
    static List<String> read(String stream, OffsetSpecification offset, int n) throws InterruptedException {
        BlockingQueue<String> got = new LinkedBlockingQueue<>();
        Consumer c = env.consumerBuilder().stream(stream).offset(offset).noTrackingStrategy()
                .messageHandler((ctx, m) -> got.add(str(m))).build();
        List<String> out = drain(got, n);
        c.close();
        return out;
    }

    /** Read until `stop` arrives (or 10 s pass without a message). */
    static List<String> readUntil(String stream, OffsetSpecification offset, String stop) throws InterruptedException {
        BlockingQueue<String> got = new LinkedBlockingQueue<>();
        Consumer c = env.consumerBuilder().stream(stream).offset(offset).noTrackingStrategy()
                .messageHandler((ctx, m) -> got.add(str(m))).build();
        List<String> out = new ArrayList<>();
        String s;
        while ((s = got.poll(10, TimeUnit.SECONDS)) != null) {
            out.add(s);
            if (s.equals(stop)) {
                break;
            }
        }
        c.close();
        return out;
    }

    static List<String> drain(BlockingQueue<String> got, int n) throws InterruptedException {
        List<String> out = new ArrayList<>();
        while (out.size() < n) {
            String s = got.poll(10, TimeUnit.SECONDS);
            if (s == null) {
                return out;
            }
            out.add(s);
        }
        String extra = got.poll(300, TimeUnit.MILLISECONDS);
        if (extra != null) {
            out.add(extra);
        }
        return out;
    }

    static List<String> range(int from, int to) {
        List<String> out = new ArrayList<>();
        for (int i = from; i < to; i++) {
            out.add("m" + i);
        }
        return out;
    }

    static void scenario(String name, Scenario s) {
        SCENARIOS.put(name, s);
    }

    static {
        scenario("create_delete", () -> {
            String s = P + ".life";
            env.streamCreator().stream(s).maxAge(Duration.ofHours(1)).maxSegmentSizeBytes(ByteCapacity.MB(1)).create();
            check(env.streamExists(s), "stream not found after create");
            env.streamCreator().stream(s).maxAge(Duration.ofHours(1)).maxSegmentSizeBytes(ByteCapacity.MB(1)).create(); // idempotent
            env.deleteStream(s);
            check(!env.streamExists(s), "stream still there after delete");
            try {
                env.queryStreamStats(s);
                throw new AssertionError("stats of a deleted stream");
            } catch (StreamDoesNotExistException expected) {
                // STREAM_DOES_NOT_EXIST
            }
            return "";
        });

        scenario("publish_confirm", () -> {
            String s = P + ".confirm";
            env.streamCreator().stream(s).create();
            publish(s, 0, 1000);
            StreamStats st = env.queryStreamStats(s);
            check(st.firstOffset() == 0 && st.committedOffset() == 999, "stats first " + st.firstOffset() + " committed " + st.committedOffset());
            env.deleteStream(s);
            return "1000 confirmed";
        });

        scenario("offsets", () -> {
            String s = P + ".offsets";
            env.streamCreator().stream(s).create();
            publish(s, 0, 10);
            long t = System.currentTimeMillis();
            Thread.sleep(1100);
            publish(s, 10, 20);
            check(read(s, OffsetSpecification.first(), 20).equals(range(0, 20)), "first");
            check(read(s, OffsetSpecification.offset(15), 5).equals(range(15, 20)), "offset 15");
            // "last" starts at the last chunk: a tail of the stream ending with the newest message.
            List<String> last = readUntil(s, OffsetSpecification.last(), "m19");
            int from = last.isEmpty() ? -1 : Integer.parseInt(last.get(0).substring(1));
            check(from >= 0 && last.equals(range(from, 20)), "last chunk " + last);
            List<String> byTime = read(s, OffsetSpecification.timestamp(t + 500), 10);
            check(byTime.equals(range(10, 20)), "timestamp " + byTime);
            BlockingQueue<String> got = new LinkedBlockingQueue<>();
            Consumer c = env.consumerBuilder().stream(s).offset(OffsetSpecification.next()).noTrackingStrategy()
                    .messageHandler((ctx, m) -> got.add(str(m) + "@" + ctx.offset())).build();
            Thread.sleep(300);
            publish(s, 20, 21);
            List<String> next = drain(got, 1);
            check(next.equals(List.of("m20@20")), "next " + next);
            c.close();
            env.deleteStream(s);
            return "";
        });

        scenario("deduplication", () -> {
            String s = P + ".dedup";
            env.streamCreator().stream(s).create();
            Producer p = env.producerBuilder().stream(s).name("app-1").build();
            List<Message> first = new ArrayList<>();
            for (int i = 1; i <= 10; i++) {
                first.add(p.messageBuilder().publishingId(i).addData(bytes("m" + i)).build());
            }
            sendAll(p, first);
            p.close();
            p = env.producerBuilder().stream(s).name("app-1").build();
            check(p.getLastPublishingId() == 10, "last publishing id " + p.getLastPublishingId());
            List<Message> again = new ArrayList<>();
            for (int i = 5; i <= 12; i++) {
                again.add(p.messageBuilder().publishingId(i).addData(bytes("m" + i)).build());
            }
            sendAll(p, again); // duplicates are confirmed but not stored
            p.close();
            List<String> all = read(s, OffsetSpecification.first(), 12);
            check(all.size() == 12 && all.get(10).equals("m11") && all.get(11).equals("m12"), "stored " + all);
            env.deleteStream(s);
            return "";
        });

        scenario("offset_tracking", () -> {
            String s = P + ".track";
            env.streamCreator().stream(s).create();
            publish(s, 0, 10);
            BlockingQueue<String> got = new LinkedBlockingQueue<>();
            Consumer c = env.consumerBuilder().stream(s).name("reader").offset(OffsetSpecification.first())
                    .manualTrackingStrategy().builder()
                    .messageHandler((ctx, m) -> {
                        got.add(str(m));
                        if (ctx.offset() == 4) {
                            ctx.storeOffset();
                        }
                    }).build();
            drain(got, 10);
            long stored = -1;
            for (int i = 0; i < 50 && stored != 4; i++) {
                try {
                    stored = c.storedOffset();
                } catch (Exception e) {
                    // not stored yet
                }
                Thread.sleep(100);
            }
            check(stored == 4, "stored offset " + stored);
            c.close();
            // A named consumer resumes after its stored offset (the offset spec is ignored).
            got.clear();
            c = env.consumerBuilder().stream(s).name("reader").offset(OffsetSpecification.first())
                    .manualTrackingStrategy().builder()
                    .messageHandler((ctx, m) -> got.add(str(m))).build();
            List<String> resumed = drain(got, 5);
            check(resumed.equals(range(5, 10)), "resumed " + resumed);
            c.close();
            env.storeOffset("other", s, 7);
            long other = -1;
            for (int i = 0; i < 50 && other != 7; i++) {
                Consumer o = env.consumerBuilder().stream(s).name("other").manualTrackingStrategy().builder().messageHandler((ctx, m) -> {}).build();
                try {
                    other = o.storedOffset();
                } catch (Exception e) {
                    // not visible yet
                }
                o.close();
                Thread.sleep(100);
            }
            check(other == 7, "environment storeOffset " + other);
            env.deleteStream(s);
            return "";
        });

        scenario("filtering", () -> {
            String s = P + ".filter";
            env.streamCreator().stream(s).create();
            Producer p = env.producerBuilder().stream(s).filterValue(m -> (String) m.getApplicationProperties().get("color")).build();
            List<Message> msgs = new ArrayList<>();
            for (String color : List.of("red", "blue", "red", "green", "red")) {
                msgs.add(p.messageBuilder().applicationProperties().entry("color", color).messageBuilder().addData(bytes(color)).build());
            }
            sendAll(p, msgs);
            p.close();
            BlockingQueue<String> got = new LinkedBlockingQueue<>();
            Consumer c = env.consumerBuilder().stream(s).offset(OffsetSpecification.first()).noTrackingStrategy()
                    .filter().values("red").postFilter(m -> "red".equals(m.getApplicationProperties().get("color"))).builder()
                    .messageHandler((ctx, m) -> got.add(str(m))).build();
            List<String> red = drain(got, 3);
            check(red.equals(List.of("red", "red", "red")), "filtered " + red);
            c.close();
            env.deleteStream(s);
            return "";
        });

        scenario("single_active_consumer", () -> {
            String s = P + ".sac";
            env.streamCreator().stream(s).create();
            BlockingQueue<String> a = new LinkedBlockingQueue<>(), b = new LinkedBlockingQueue<>();
            Consumer ca = env.consumerBuilder().stream(s).name("group").singleActiveConsumer().offset(OffsetSpecification.first())
                    .manualTrackingStrategy().builder().messageHandler((ctx, m) -> a.add(str(m))).build();
            Thread.sleep(300);
            Consumer cb = env.consumerBuilder().stream(s).name("group").singleActiveConsumer().offset(OffsetSpecification.first())
                    .manualTrackingStrategy().builder().messageHandler((ctx, m) -> b.add(str(m))).build();
            publish(s, 0, 5);
            check(drain(a, 5).equals(range(0, 5)), "active consumer");
            check(b.poll(500, TimeUnit.MILLISECONDS) == null, "inactive consumer got messages");
            ca.close();
            publish(s, 5, 6);
            String took = b.poll(10, TimeUnit.SECONDS);
            check(took != null, "second consumer did not take over");
            cb.close();
            env.deleteStream(s);
            return "";
        });

        scenario("super_stream", () -> {
            String s = P + ".super";
            env.streamCreator().name(s).superStream().partitions(3).creator().create();
            Producer p = env.producerBuilder().superStream(s).routing(m -> m.getProperties().getMessageIdAsString()).producerBuilder().build();
            List<Message> msgs = new ArrayList<>();
            for (int i = 0; i < 30; i++) {
                msgs.add(p.messageBuilder().properties().messageId("id-" + i).messageBuilder().addData(bytes("m" + i)).build());
            }
            sendAll(p, msgs);
            p.close();
            int used = 0;
            for (int i = 0; i < 3; i++) {
                long n = env.queryStreamStats(s + "-" + i).committedOffset() + 1;
                if (n > 0) {
                    used++;
                }
            }
            check(used >= 2, "messages spread over " + used + " partitions");
            BlockingQueue<String> got = new LinkedBlockingQueue<>();
            Consumer c = env.consumerBuilder().superStream(s).offset(OffsetSpecification.first()).noTrackingStrategy()
                    .messageHandler((ctx, m) -> got.add(str(m))).build();
            List<String> all = drain(got, 30);
            Collections.sort(all);
            List<String> want = range(0, 30);
            Collections.sort(want);
            check(all.equals(want), "consumed " + all.size());
            c.close();
            env.deleteSuperStream(s);
            return "3 partitions";
        });

        scenario("sub_entry_compression", () -> {
            String s = P + ".gzip";
            env.streamCreator().stream(s).create();
            Producer p = env.producerBuilder().stream(s).subEntrySize(10).compression(Compression.GZIP).build();
            sendAll(p, texts(p, 0, 100));
            p.close();
            check(read(s, OffsetSpecification.first(), 100).equals(range(0, 100)), "decompressed sub-entries");
            env.deleteStream(s);
            return "100 in gzip sub-entries";
        });

        scenario("retention", () -> {
            String s = P + ".retention";
            env.streamCreator().stream(s).maxLengthBytes(ByteCapacity.kB(200)).maxSegmentSizeBytes(ByteCapacity.kB(50)).create();
            Producer p = env.producerBuilder().stream(s).build();
            byte[] body = new byte[1000];
            List<Message> msgs = new ArrayList<>();
            for (int i = 0; i < 1000; i++) {
                msgs.add(p.messageBuilder().addData(body).build());
            }
            sendAll(p, msgs);
            p.close();
            long first = 0;
            for (int i = 0; i < 50 && first == 0; i++) {
                first = env.queryStreamStats(s).firstOffset();
                Thread.sleep(100);
            }
            check(first > 0, "old segments kept: first offset " + first);
            env.deleteStream(s);
            return "first offset " + first;
        });

        scenario("amqp091_interop", () -> {
            String s = P + ".interop";
            com.rabbitmq.client.ConnectionFactory f = new com.rabbitmq.client.ConnectionFactory();
            f.setUri(AMQP_URL);
            try (com.rabbitmq.client.Connection old = f.newConnection()) {
                com.rabbitmq.client.Channel ch = old.createChannel();
                ch.queueDeclare(s, true, false, false, Map.of("x-queue-type", "stream"));
                ch.confirmSelect();
                Map<String, Object> headers = new HashMap<>();
                headers.put("k", "v");
                ch.basicPublish("", s, new com.rabbitmq.client.AMQP.BasicProperties.Builder()
                        .headers(headers).messageId("m-1").contentType("text/plain").build(), bytes("from 0-9-1"));
                ch.waitForConfirmsOrDie(5000);
                BlockingQueue<Message> got = new LinkedBlockingQueue<>();
                Consumer c = env.consumerBuilder().stream(s).offset(OffsetSpecification.first()).noTrackingStrategy()
                        .messageHandler((ctx, m) -> got.add(m)).build();
                Message m = got.poll(10, TimeUnit.SECONDS);
                check(m != null, "nothing from the stream protocol");
                check(str(m).equals("from 0-9-1"), "body " + str(m));
                check("v".equals(String.valueOf(m.getApplicationProperties().get("k"))), "header as application property " + m.getApplicationProperties());
                check("m-1".equals(m.getProperties().getMessageIdAsString()), "message-id " + m.getProperties().getMessageId());
                c.close();
                // And back: the stream protocol publishes, a 0-9-1 consumer reads with an offset.
                publish(s, 0, 1);
                ch.basicQos(10);
                BlockingQueue<String> old091 = new LinkedBlockingQueue<>();
                ch.basicConsume(s, false, Map.of("x-stream-offset", "first"), new com.rabbitmq.client.DefaultConsumer(ch) {
                    @Override
                    public void handleDelivery(String tag, com.rabbitmq.client.Envelope e, com.rabbitmq.client.AMQP.BasicProperties props, byte[] body) throws java.io.IOException {
                        old091.add(new String(body, StandardCharsets.UTF_8));
                        ch.basicAck(e.getDeliveryTag(), false);
                    }
                });
                List<String> both = drain(old091, 2);
                check(both.equals(List.of("from 0-9-1", "m0")), "0-9-1 consumer read " + both);
                ch.queueDelete(s);
            }
            return "";
        });

        scenario("throughput", () -> {
            String s = P + ".tp";
            env.streamCreator().stream(s).create();
            int n = 200_000;
            Producer p = env.producerBuilder().stream(s).build();
            CountDownLatch confirmed = new CountDownLatch(n);
            byte[] body = new byte[64];
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                p.send(p.messageBuilder().addData(body).build(), st -> confirmed.countDown());
            }
            check(confirmed.await(60, TimeUnit.SECONDS), "confirms outstanding " + confirmed.getCount());
            double pub = (System.nanoTime() - t0) / 1e9;
            p.close();
            CountDownLatch got = new CountDownLatch(n);
            long t1 = System.nanoTime();
            Consumer c = env.consumerBuilder().stream(s).offset(OffsetSpecification.first()).noTrackingStrategy()
                    .messageHandler((ctx, m) -> got.countDown()).build();
            check(got.await(60, TimeUnit.SECONDS), "consumed short by " + got.getCount());
            double con = (System.nanoTime() - t1) / 1e9;
            c.close();
            env.deleteStream(s);
            return String.format("publish %.0f msg/s, consume %.0f msg/s", n / pub, n / con);
        });
    }

    static String describe(Throwable t) {
        StringBuilder sb = new StringBuilder(t.getClass().getSimpleName() + ": " + t.getMessage());
        for (Throwable c = t.getCause(); c != null && c != c.getCause(); c = c.getCause()) {
            sb.append(" <- ").append(c.getClass().getSimpleName()).append(": ").append(c.getMessage());
        }
        return sb.toString().replace('\n', ' ');
    }

    public static void main(String[] args) {
        Set<String> only = Set.of(args);
        env = Environment.builder().host(HOST).port(PORT).username("guest").password("guest").build();
        ExecutorService pool = Executors.newCachedThreadPool(r -> {
            Thread t = new Thread(r);
            t.setDaemon(true);
            return t;
        });
        int failed = 0;
        for (Map.Entry<String, Scenario> e : SCENARIOS.entrySet()) {
            String name = e.getKey();
            if (!only.isEmpty() && !only.contains(name)) {
                continue;
            }
            long t = System.nanoTime();
            Future<String> f = pool.submit(() -> e.getValue().run());
            try {
                String detail = f.get(name.equals("throughput") ? 120 : 30, TimeUnit.SECONDS);
                String secs = String.format("(%.1fs)", (System.nanoTime() - t) / 1e9);
                System.out.println(("PASS " + name + " " + detail + " " + secs).replace("  ", " "));
            } catch (TimeoutException te) {
                failed++;
                f.cancel(true);
                System.out.println("FAIL " + name + ": scenario timed out");
            } catch (Throwable ex) {
                failed++;
                Throwable cause = ex instanceof ExecutionException && ex.getCause() != null ? ex.getCause() : ex;
                System.out.println("FAIL " + name + ": " + describe(cause));
                cause.printStackTrace(System.err);
            }
            System.out.flush();
        }
        env.close();
        System.exit(failed > 0 ? 1 : 0);
    }
}
