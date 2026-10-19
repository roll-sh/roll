package roll.qtest;

import com.rabbitmq.client.amqp.AmqpException;
import com.rabbitmq.client.amqp.ByteCapacity;
import com.rabbitmq.client.amqp.Connection;
import com.rabbitmq.client.amqp.Consumer;
import com.rabbitmq.client.amqp.ConsumerBuilder;
import com.rabbitmq.client.amqp.Environment;
import com.rabbitmq.client.amqp.Management;
import com.rabbitmq.client.amqp.Message;
import com.rabbitmq.client.amqp.Publisher;
import com.rabbitmq.client.amqp.Requester;
import com.rabbitmq.client.amqp.Responder;
import com.rabbitmq.client.amqp.impl.AmqpEnvironmentBuilder;
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
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicInteger;

/** roll queues compatibility scenarios for RabbitMQ's AMQP 1.0 Java client (com.rabbitmq.client:amqp-client). */
public final class Main {
    static final String URL = System.getenv().getOrDefault("AMQP_URL", "amqp://guest:guest@localhost:5672/%2F");
    static final String P = "amqp10-" + UUID.randomUUID().toString().substring(0, 6);
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

    static Connection connect() {
        return env.connectionBuilder().build();
    }

    static byte[] bytes(String s) {
        return s.getBytes(StandardCharsets.UTF_8);
    }

    static String str(Message m) {
        return new String(m.body(), StandardCharsets.UTF_8);
    }

    /** Publish and wait for the outcome. */
    static Publisher.Status publish(Publisher p, Message m) throws Exception {
        CompletableFuture<Publisher.Status> f = new CompletableFuture<>();
        p.publish(m, ctx -> f.complete(ctx.status()));
        return f.get(10, TimeUnit.SECONDS);
    }

    record Received(Consumer.Context ctx, Message msg) {}

    static Consumer consume(Connection c, String queue, BlockingQueue<Received> into) {
        return c.consumerBuilder().queue(queue).messageHandler((ctx, m) -> into.add(new Received(ctx, m))).build();
    }

    static Received take(BlockingQueue<Received> q) throws InterruptedException {
        Received r = q.poll(10, TimeUnit.SECONDS);
        check(r != null, "no message within 10s");
        return r;
    }

    static void quiet(BlockingQueue<Received> q) throws InterruptedException {
        Received r = q.poll(500, TimeUnit.MILLISECONDS);
        check(r == null, "unexpected message " + (r == null ? "" : str(r.msg())));
    }

    static long count(Management m, String q) {
        return m.queueInfo(q).messageCount();
    }

    static long countEventually(Management m, String q, long want) throws InterruptedException {
        long n = -1;
        for (int i = 0; i < 100; i++) {
            n = count(m, q);
            if (n == want) {
                return n;
            }
            Thread.sleep(50);
        }
        return n;
    }

    static void scenario(String name, Scenario s) {
        SCENARIOS.put(name, s);
    }

    static {
        scenario("connect", () -> {
            try (Connection c = connect()) {
                Connection.ConnectionInfo i = c.connectionInfo();
                check(i.brokerProductName() != null && i.brokerVersion() != null, "no broker info");
                return i.brokerProductName() + " " + i.brokerVersion();
            }
        });

        scenario("topology", () -> {
            try (Connection c = connect()) {
                Management m = c.management();
                String q = P + ".topo", x = P + ".topo.x", x2 = P + ".topo.x2";
                m.exchange(x).type(Management.ExchangeType.TOPIC).declare();
                m.exchange(x2).type(Management.ExchangeType.FANOUT).declare();
                Management.QueueInfo info = m.queue(q).type(Management.QueueType.CLASSIC).messageTtl(Duration.ofMinutes(5)).maxLength(100).declare();
                check(info.name().equals(q) && info.type() == Management.QueueType.CLASSIC && info.durable(), "queue info " + info.type() + " durable=" + info.durable());
                check(Duration.ofMinutes(5).toMillis() == ((Number) info.arguments().get("x-message-ttl")).longValue(), "arguments " + info.arguments());
                m.binding().sourceExchange(x).destinationExchange(x2).key("a.#").bind();
                m.binding().sourceExchange(x2).destinationQueue(q).bind();
                try (Publisher p = c.publisherBuilder().exchange(x).key("a.b").build()) {
                    check(publish(p, p.message(bytes("routed"))) == Publisher.Status.ACCEPTED, "routed through e2e binding");
                }
                check(countEventually(m, q, 1) == 1, "message count " + count(m, q));
                m.unbind().sourceExchange(x).destinationExchange(x2).key("a.#").unbind();
                try (Publisher p = c.publisherBuilder().exchange(x).key("a.b").build()) {
                    check(publish(p, p.message(bytes("unrouted"))) == Publisher.Status.RELEASED, "unroutable after unbind");
                }
                check(m.queuePurge(q).messageCount() == 1, "purge count");
                m.queueDelete(q);
                m.exchangeDelete(x);
                m.exchangeDelete(x2);
                try {
                    m.queueInfo(q);
                    throw new AssertionError("queue still exists");
                } catch (AmqpException.AmqpEntityDoesNotExistException expected) {
                    // RabbitMQ answers 404
                }
                return "";
            }
        });

        scenario("declare_mismatch", () -> {
            try (Connection c = connect()) {
                Management m = c.management();
                String q = P + ".mismatch";
                m.queue(q).type(Management.QueueType.QUORUM).declare();
                try {
                    m.queue(q).type(Management.QueueType.CLASSIC).declare();
                    throw new AssertionError("redeclare with another type succeeded");
                } catch (AmqpException e) {
                    check(!(e instanceof AmqpException.AmqpEntityDoesNotExistException), "wrong error " + e);
                }
                m.queueDelete(q);
                return "";
            }
        });

        scenario("publish_outcomes", () -> {
            try (Connection c = connect()) {
                Management m = c.management();
                String q = P + ".full";
                m.queue(q).maxLength(1).overflowStrategy(Management.OverflowStrategy.REJECT_PUBLISH).declare();
                try (Publisher p = c.publisherBuilder().queue(q).build()) {
                    check(publish(p, p.message(bytes("1"))) == Publisher.Status.ACCEPTED, "first accepted");
                    Publisher.Status s = publish(p, p.message(bytes("2")));
                    check(s == Publisher.Status.REJECTED, "full queue: " + s);
                }
                try (Publisher p = c.publisherBuilder().exchange("amq.direct").key(P + ".nowhere").build()) {
                    check(publish(p, p.message(bytes("x"))) == Publisher.Status.RELEASED, "unroutable");
                }
                try {
                    c.publisherBuilder().queue(P + ".missing").build();
                    throw new AssertionError("publisher to a missing queue");
                } catch (AmqpException.AmqpEntityDoesNotExistException expected) {
                    // 404
                }
                m.queueDelete(q);
                return "";
            }
        });

        scenario("consume_accept", () -> {
            try (Connection c = connect()) {
                Management m = c.management();
                String q = P + ".accept";
                m.queue(q).declare();
                try (Publisher p = c.publisherBuilder().queue(q).build()) {
                    for (int i = 0; i < 10; i++) {
                        check(publish(p, p.message(bytes("m" + i)).messageId((long) i)) == Publisher.Status.ACCEPTED, "accepted");
                    }
                }
                BlockingQueue<Received> got = new LinkedBlockingQueue<>();
                try (Consumer consumer = consume(c, q, got)) {
                    for (int i = 0; i < 10; i++) {
                        Received r = take(got);
                        check(str(r.msg()).equals("m" + i) && String.valueOf(i).equals(String.valueOf(r.msg().messageId())), "order at " + i + ": " + str(r.msg()) + " id " + r.msg().messageId());
                        r.ctx().accept();
                    }
                }
                check(countEventually(m, q, 0) == 0, "left " + count(m, q));
                m.queueDelete(q);
                return "";
            }
        });

        scenario("requeue", () -> {
            try (Connection c = connect()) {
                Management m = c.management();
                String q = P + ".requeue";
                m.queue(q).type(Management.QueueType.QUORUM).declare();
                try (Publisher p = c.publisherBuilder().queue(q).build()) {
                    publish(p, p.message(bytes("again")));
                }
                BlockingQueue<Received> got = new LinkedBlockingQueue<>();
                try (Consumer consumer = consume(c, q, got)) {
                    Received r = take(got);
                    r.ctx().requeue(Map.of("x-opt-retried", true), true);
                    Received r2 = take(got);
                    check(str(r2.msg()).equals("again"), "requeued body");
                    check(Boolean.TRUE.equals(r2.msg().annotation("x-opt-retried")), "requeue annotations " + r2.msg().annotation("x-opt-retried"));
                    Object dc = r2.msg().annotation("x-delivery-count");
                    check(dc != null && ((Number) dc).longValue() == 1, "x-delivery-count " + dc);
                    r2.ctx().accept();
                }
                m.queueDelete(q);
                return "";
            }
        });

        scenario("discard_dead_letter", () -> {
            try (Connection c = connect()) {
                Management m = c.management();
                String q = P + ".dl", dlq = P + ".dlq";
                m.queue(dlq).declare();
                m.queue(q).deadLetterExchange("").deadLetterRoutingKey(dlq).declare();
                try (Publisher p = c.publisherBuilder().queue(q).build()) {
                    publish(p, p.message(bytes("dead")));
                }
                BlockingQueue<Received> got = new LinkedBlockingQueue<>();
                try (Consumer consumer = consume(c, q, got)) {
                    take(got).ctx().discard();
                }
                BlockingQueue<Received> dead = new LinkedBlockingQueue<>();
                try (Consumer consumer = consume(c, dlq, dead)) {
                    Received r = take(dead);
                    check(str(r.msg()).equals("dead"), "dead-lettered body");
                    r.ctx().accept();
                }
                m.queueDelete(q);
                m.queueDelete(dlq);
                return "";
            }
        });

        scenario("quorum_delivery_limit", () -> {
            try (Connection c = connect()) {
                Management m = c.management();
                String q = P + ".qlimit";
                Management.QueueInfo info = m.queue(q).quorum().deliveryLimit(1).queue().declare();
                check(info.type() == Management.QueueType.QUORUM, "type " + info.type());
                try (Publisher p = c.publisherBuilder().queue(q).build()) {
                    publish(p, p.message(bytes("poison")));
                }
                BlockingQueue<Received> got = new LinkedBlockingQueue<>();
                try (Consumer consumer = consume(c, q, got)) {
                    take(got).ctx().requeue(Collections.emptyMap(), true);
                    take(got).ctx().requeue(Collections.emptyMap(), true);
                    quiet(got);
                }
                check(countEventually(m, q, 0) == 0, "poison message still there");
                m.queueDelete(q);
                return "";
            }
        });

        scenario("stream_offsets", () -> {
            try (Connection c = connect()) {
                Management m = c.management();
                String s = P + ".stream";
                m.queue(s).type(Management.QueueType.STREAM).stream().maxSegmentSizeBytes(ByteCapacity.MB(1)).queue().declare();
                try (Publisher p = c.publisherBuilder().queue(s).build()) {
                    for (int i = 0; i < 10; i++) {
                        publish(p, p.message(bytes("s" + i)));
                    }
                }
                List<String> first = read(c, s, b -> b.offset(ConsumerBuilder.StreamOffsetSpecification.FIRST), 10);
                check(first.equals(range(0, 10)), "from first: " + first);
                List<String> from5 = read(c, s, b -> b.offset(5), 5);
                check(from5.equals(range(5, 10)), "from offset 5: " + from5);
                // A stream is a log: reading never removes anything.
                List<String> again = read(c, s, b -> b.offset(ConsumerBuilder.StreamOffsetSpecification.FIRST), 10);
                check(again.equals(range(0, 10)), "replay: " + again);
                BlockingQueue<Received> got = new LinkedBlockingQueue<>();
                try (Consumer consumer = c.consumerBuilder().queue(s).stream().offset(ConsumerBuilder.StreamOffsetSpecification.NEXT).builder()
                        .messageHandler((ctx, msg) -> {
                            ctx.accept();
                            got.add(new Received(ctx, msg));
                        }).build()) {
                    Thread.sleep(300);
                    try (Publisher p = c.publisherBuilder().queue(s).build()) {
                        publish(p, p.message(bytes("new")));
                    }
                    Received r = take(got);
                    check(str(r.msg()).equals("new"), "next: " + str(r.msg()));
                    Object off = r.msg().annotation("x-stream-offset");
                    check(off != null && ((Number) off).longValue() == 10, "x-stream-offset " + off);
                }
                m.queueDelete(s);
                return "";
            }
        });

        scenario("stream_filter", () -> {
            try (Connection c = connect()) {
                Management m = c.management();
                String s = P + ".sfilter";
                m.queue(s).type(Management.QueueType.STREAM).declare();
                try (Publisher p = c.publisherBuilder().queue(s).build()) {
                    for (String color : List.of("red", "blue", "red", "green")) {
                        publish(p, p.message(bytes(color)).annotation("x-stream-filter-value", color));
                    }
                }
                List<String> red = read(c, s, b -> b.offset(ConsumerBuilder.StreamOffsetSpecification.FIRST).filterValues("red"), 2);
                check(red.equals(List.of("red", "red")), "filtered: " + red);
                m.queueDelete(s);
                return "";
            }
        });

        scenario("properties", () -> {
            try (Connection c = connect()) {
                Management m = c.management();
                String q = P + ".props";
                m.queue(q).declare();
                UUID id = UUID.randomUUID();
                try (Publisher p = c.publisherBuilder().build()) {
                    Message msg = p.message(bytes("{}"))
                            .messageId(id)
                            .correlationId("corr-1")
                            .subject("subj")
                            .contentType("application/json")
                            .replyTo("/queues/" + q)
                            .property("k", "v")
                            .property("n", 42L)
                            .property("b", true)
                            .toAddress().queue(q).message();
                    check(publish(p, msg) == Publisher.Status.ACCEPTED, "per-message address");
                }
                BlockingQueue<Received> got = new LinkedBlockingQueue<>();
                try (Consumer consumer = consume(c, q, got)) {
                    Received r = take(got);
                    Message msg = r.msg();
                    check(id.equals(msg.messageId()), "message-id " + msg.messageId());
                    check("corr-1".equals(msg.correlationId()), "correlation-id " + msg.correlationId());
                    check("subj".equals(msg.subject()) && "application/json".equals(msg.contentType()), "subject/content-type " + msg.subject() + " " + msg.contentType());
                    check(("/queues/" + q).equals(msg.replyTo()), "reply-to " + msg.replyTo());
                    check("v".equals(msg.property("k")) && Long.valueOf(42).equals(msg.property("n")) && Boolean.TRUE.equals(msg.property("b")), "application properties " + msg.property("k") + " " + msg.property("n") + " " + msg.property("b"));
                    r.ctx().accept();
                }
                m.queueDelete(q);
                return "";
            }
        });

        scenario("rpc", () -> {
            try (Connection c = connect()) {
                Management m = c.management();
                String q = P + ".rpc";
                m.queue(q).declare();
                try (Responder responder = c.responderBuilder().requestQueue(q)
                        .handler((ctx, req) -> ctx.message(bytes(str(req).toUpperCase()))).build();
                     Requester requester = c.requesterBuilder().requestAddress().queue(q).requester().build()) {
                    Message reply = requester.publish(requester.message(bytes("ping"))).get(10, TimeUnit.SECONDS);
                    check(str(reply).equals("PING"), "reply " + str(reply));
                }
                m.queueDelete(q);
                return "";
            }
        });

        scenario("single_active_consumer", () -> {
            try (Connection c = connect()) {
                Management m = c.management();
                String q = P + ".sac";
                m.queue(q).type(Management.QueueType.QUORUM).singleActiveConsumer(true).declare();
                BlockingQueue<Received> a = new LinkedBlockingQueue<>(), b = new LinkedBlockingQueue<>();
                Consumer ca = consume(c, q, a);
                Consumer cb = consume(c, q, b);
                try (Publisher p = c.publisherBuilder().queue(q).build()) {
                    for (int i = 0; i < 4; i++) {
                        publish(p, p.message(bytes("" + i)));
                    }
                }
                for (int i = 0; i < 4; i++) {
                    take(a).ctx().accept();
                }
                quiet(b);
                ca.close();
                try (Publisher p = c.publisherBuilder().queue(q).build()) {
                    publish(p, p.message(bytes("after")));
                }
                Received r = take(b);
                check(str(r.msg()).equals("after"), "second consumer took over");
                r.ctx().accept();
                cb.close();
                m.queueDelete(q);
                return "";
            }
        });

        scenario("amqp091_interop", () -> {
            try (Connection c = connect()) {
                Management m = c.management();
                String q = P + ".interop";
                m.queue(q).declare();
                com.rabbitmq.client.ConnectionFactory f = new com.rabbitmq.client.ConnectionFactory();
                f.setUri(URL);
                try (com.rabbitmq.client.Connection old = f.newConnection()) {
                    com.rabbitmq.client.Channel ch = old.createChannel();
                    Map<String, Object> headers = new HashMap<>();
                    headers.put("k", "v");
                    ch.basicPublish("", q, new com.rabbitmq.client.AMQP.BasicProperties.Builder()
                            .headers(headers).contentType("text/plain").messageId("m-1").correlationId("c-1").build(), bytes("from 0-9-1"));
                    BlockingQueue<Received> got = new LinkedBlockingQueue<>();
                    try (Consumer consumer = consume(c, q, got)) {
                        Received r = take(got);
                        Message msg = r.msg();
                        check(str(msg).equals("from 0-9-1"), "body " + str(msg));
                        check("v".equals(msg.property("k")), "header as application property " + msg.property("k"));
                        check("m-1".equals(msg.messageId()) && "c-1".equals(msg.correlationId()) && "text/plain".equals(msg.contentType()), "properties " + msg.messageId() + " " + msg.correlationId() + " " + msg.contentType());
                        r.ctx().accept();
                    }
                    try (Publisher p = c.publisherBuilder().queue(q).build()) {
                        publish(p, p.message(bytes("from 1.0")).property("k", "w").messageId("m-2").subject("s"));
                    }
                    com.rabbitmq.client.GetResponse g = null;
                    for (int i = 0; i < 100 && g == null; i++) {
                        g = ch.basicGet(q, true);
                        if (g == null) {
                            Thread.sleep(50);
                        }
                    }
                    check(g != null, "0-9-1 basic.get got nothing");
                    check(new String(g.getBody(), StandardCharsets.UTF_8).equals("from 1.0"), "0-9-1 body " + new String(g.getBody(), StandardCharsets.UTF_8));
                    Object h = g.getProps().getHeaders() == null ? null : g.getProps().getHeaders().get("k");
                    check(h != null && h.toString().equals("w"), "application property as header " + h);
                    check("m-2".equals(g.getProps().getMessageId()), "message-id " + g.getProps().getMessageId());
                }
                m.queueDelete(q);
                return "";
            }
        });

        scenario("throughput", () -> {
            try (Connection c = connect()) {
                Management m = c.management();
                String q = P + ".tp";
                m.queue(q).declare();
                int n = 20000;
                CountDownLatch confirmed = new CountDownLatch(n);
                AtomicInteger rejected = new AtomicInteger();
                long t0 = System.nanoTime();
                try (Publisher p = c.publisherBuilder().queue(q).build()) {
                    byte[] body = new byte[64];
                    for (int i = 0; i < n; i++) {
                        p.publish(p.message(body), ctx -> {
                            if (ctx.status() != Publisher.Status.ACCEPTED) {
                                rejected.incrementAndGet();
                            }
                            confirmed.countDown();
                        });
                    }
                    check(confirmed.await(30, TimeUnit.SECONDS), "confirms outstanding " + confirmed.getCount());
                }
                double pub = (System.nanoTime() - t0) / 1e9;
                check(rejected.get() == 0, rejected.get() + " not accepted");
                CountDownLatch got = new CountDownLatch(n);
                long t1 = System.nanoTime();
                try (Consumer consumer = c.consumerBuilder().queue(q).initialCredits(1000).messageHandler((ctx, msg) -> {
                    ctx.accept();
                    got.countDown();
                }).build()) {
                    check(got.await(30, TimeUnit.SECONDS), "consumed short by " + got.getCount());
                }
                double con = (System.nanoTime() - t1) / 1e9;
                m.queueDelete(q);
                return String.format("publish %.0f msg/s, consume %.0f msg/s", n / pub, n / con);
            }
        });
    }

    interface Offset {
        ConsumerBuilder.StreamOptions apply(ConsumerBuilder.StreamOptions b);
    }

    static List<String> read(Connection c, String stream, Offset offset, int n) throws InterruptedException {
        BlockingQueue<String> got = new LinkedBlockingQueue<>();
        try (Consumer consumer = offset.apply(c.consumerBuilder().queue(stream).stream()).builder()
                .messageHandler((ctx, msg) -> {
                    ctx.accept();
                    got.add(str(msg));
                }).build()) {
            List<String> out = new ArrayList<>();
            while (out.size() < n) {
                String s = got.poll(10, TimeUnit.SECONDS);
                if (s == null) {
                    break;
                }
                out.add(s);
            }
            String extra = got.poll(300, TimeUnit.MILLISECONDS);
            if (extra != null) {
                out.add(extra);
            }
            return out;
        }
    }

    static List<String> range(int from, int to) {
        List<String> out = new ArrayList<>();
        for (int i = from; i < to; i++) {
            out.add("s" + i);
        }
        return out;
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
        env = new AmqpEnvironmentBuilder().connectionSettings().uri(URL).environmentBuilder().build();
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
                String detail = f.get(name.equals("throughput") ? 60 : 20, TimeUnit.SECONDS);
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
