package roll.qtest;

import com.rabbitmq.client.AMQP;
import com.rabbitmq.client.BlockedListener;
import com.rabbitmq.client.Channel;
import com.rabbitmq.client.Connection;
import com.rabbitmq.client.ConnectionFactory;
import com.rabbitmq.client.DefaultConsumer;
import com.rabbitmq.client.Envelope;
import com.rabbitmq.client.GetResponse;
import com.rabbitmq.client.MessageProperties;
import com.rabbitmq.client.ShutdownSignalException;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.SecureRandom;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicInteger;

/** roll queues compatibility scenarios for the RabbitMQ Java client (com.rabbitmq:amqp-client). */
public final class Main {
    static final String URL = System.getenv().getOrDefault("AMQP_URL", "amqp://guest:guest@localhost:5672/%2F");
    static final String P = "java-" + UUID.randomUUID().toString().replace("-", "").substring(0, 6);
    static final Map<String, Scenario> SCENARIOS = new LinkedHashMap<>();

    interface Scenario {
        String run() throws Exception;
    }

    interface Action {
        void run() throws Exception;
    }

    static Connection connect() throws Exception {
        return connect(-1);
    }

    static Connection connect(int heartbeat) throws Exception {
        ConnectionFactory f = new ConnectionFactory();
        f.setUri(URL);
        f.setAutomaticRecoveryEnabled(false);
        f.setConnectionTimeout(5000);
        f.setHandshakeTimeout(5000);
        f.setChannelRpcTimeout(10000);
        f.setShutdownTimeout(1000);
        if (heartbeat >= 0) {
            f.setRequestedHeartbeat(heartbeat);
        }
        return f.newConnection("roll-qtest-java");
    }

    static void check(boolean cond, String msg) {
        if (!cond) {
            throw new AssertionError(msg);
        }
    }

    static byte[] b(String s) {
        return s.getBytes(StandardCharsets.UTF_8);
    }

    static String s(byte[] b) {
        return new String(b, StandardCharsets.UTF_8);
    }

    static int count(Channel ch, String q) throws IOException {
        return ch.queueDeclarePassive(q).getMessageCount();
    }

    static GetResponse getWait(Channel ch, String q) throws Exception {
        long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (System.nanoTime() < end) {
            GetResponse r = ch.basicGet(q, false);
            if (r != null) {
                return r;
            }
            Thread.sleep(50);
        }
        throw new AssertionError("no message on " + q + " within 5s");
    }

    /** Runs fn, expects a broker channel.close with the given reply code, returns its reply text. */
    static String expectClose(Action fn, int code) throws Exception {
        ShutdownSignalException sse;
        try {
            fn.run();
            throw new AssertionError("expected channel error " + code);
        } catch (IOException e) {
            if (!(e.getCause() instanceof ShutdownSignalException c)) {
                throw e;
            }
            sse = c;
        } catch (ShutdownSignalException e) {
            sse = e;
        }
        if (!(sse.getReason() instanceof AMQP.Channel.Close close)) {
            throw new AssertionError("expected channel.close " + code + ", got " + sse.getReason());
        }
        check(close.getReplyCode() == code, "expected " + code + ", got " + close.getReplyCode() + " " + close.getReplyText());
        return close.getReplyText();
    }

    static void scenario(String name, Scenario s) {
        SCENARIOS.put(name, s);
    }

    static {
        scenario("connect", () -> {
            try (Connection c = connect()) {
                Map<String, Object> props = c.getServerProperties();
                check(c.isOpen(), "connection not open");
                Channel ch = c.createChannel();
                check(ch.isOpen(), "channel not open");
                return props.get("product") + " " + props.get("version");
            }
        });

        scenario("declare", () -> {
            try (Connection c = connect()) {
                Channel ch = c.createChannel();
                String x = P + ".declare.x", q = P + ".declare.q";
                ch.exchangeDeclare(x, "direct", true);
                AMQP.Queue.DeclareOk ok = ch.queueDeclare(q, true, false, false, null);
                check(ok.getQueue().equals(q) && ok.getMessageCount() == 0, "declare-ok " + ok);
                ch.queueBind(q, x, "k");
                ch.exchangeDeclarePassive(x);
                String srv = ch.queueDeclare().getQueue();
                check(srv.startsWith("amq.gen-"), "server-named queue " + srv);
                Channel ch1 = ch;
                String text = expectClose(() -> ch1.queueDeclare(q, false, false, false, null), 406);
                check(text.contains("inequivalent arg 'durable'"), text);
                Channel ch2 = c.createChannel();
                expectClose(() -> ch2.queueDeclarePassive(P + ".missing"), 404);
                ch = c.createChannel();
                ch.queueUnbind(q, x, "k");
                ch.queueDelete(q);
                ch.exchangeDelete(x);
                return "";
            }
        });

        scenario("publish_confirms", () -> {
            try (Connection c = connect()) {
                Channel ch = c.createChannel();
                String q = P + ".confirms";
                ch.queueDeclare(q, true, false, false, null);
                List<String> returned = Collections.synchronizedList(new ArrayList<>());
                CountDownLatch returnedLatch = new CountDownLatch(1);
                ch.addReturnListener(r -> {
                    returned.add(r.getReplyCode() + " " + r.getRoutingKey());
                    returnedLatch.countDown();
                });
                ch.confirmSelect();
                for (int i = 0; i < 100; i++) {
                    ch.basicPublish("", q, true, MessageProperties.PERSISTENT_BASIC, b("m" + i));
                }
                ch.waitForConfirmsOrDie(10_000);
                check(count(ch, q) == 100, "count " + count(ch, q));
                check(returned.isEmpty(), "routable messages were returned: " + returned);
                ch.basicPublish("", P + ".nowhere", true, null, b("x"));
                ch.waitForConfirmsOrDie(10_000);
                check(returnedLatch.await(5, TimeUnit.SECONDS), "unroutable mandatory publish was not returned");
                check(returned.equals(List.of("312 " + P + ".nowhere")), "returns " + returned);
                ch.queueDelete(q);
                return "100 acked, mandatory return ok";
            }
        });

        scenario("consume_ack", () -> {
            try (Connection c = connect()) {
                Channel ch = c.createChannel();
                String q = P + ".consume";
                ch.queueDeclare(q, false, false, false, null);
                ch.confirmSelect();
                for (int i = 0; i < 10; i++) {
                    ch.basicPublish("", q, null, b("m" + i));
                }
                ch.waitForConfirmsOrDie(10_000);
                List<String> got = Collections.synchronizedList(new ArrayList<>());
                CountDownLatch done = new CountDownLatch(10);
                String tag = ch.basicConsume(q, false, new DefaultConsumer(ch) {
                    @Override
                    public void handleDelivery(String t, Envelope env, AMQP.BasicProperties p, byte[] body) throws IOException {
                        got.add(s(body));
                        getChannel().basicAck(env.getDeliveryTag(), false);
                        done.countDown();
                    }
                });
                check(done.await(5, TimeUnit.SECONDS), "timed out after " + got.size());
                ch.basicCancel(tag);
                List<String> want = new ArrayList<>();
                for (int i = 0; i < 10; i++) {
                    want.add("m" + i);
                }
                check(got.equals(want), "order " + got);
                check(count(ch, q) == 0, "queue not empty after acks");
                ch.queueDelete(q);
                return "";
            }
        });

        scenario("nack_requeue", () -> {
            try (Connection c = connect()) {
                Channel ch = c.createChannel();
                String q = P + ".nack";
                ch.queueDeclare(q, false, false, false, null);
                ch.basicPublish("", q, null, b("again"));
                CompletableFuture<Void> done = new CompletableFuture<>();
                AtomicInteger n = new AtomicInteger();
                String tag = ch.basicConsume(q, false, new DefaultConsumer(ch) {
                    @Override
                    public void handleDelivery(String t, Envelope env, AMQP.BasicProperties p, byte[] body) throws IOException {
                        try {
                            if (n.incrementAndGet() == 1) {
                                check(!env.isRedeliver(), "first delivery flagged redelivered");
                                getChannel().basicNack(env.getDeliveryTag(), false, true);
                            } else {
                                check(env.isRedeliver() && s(body).equals("again"), "requeued delivery not redelivered");
                                getChannel().basicAck(env.getDeliveryTag(), false);
                                done.complete(null);
                            }
                        } catch (Throwable e) {
                            done.completeExceptionally(e);
                        }
                    }
                });
                done.get(5, TimeUnit.SECONDS);
                ch.basicCancel(tag);
                ch.queueDelete(q);
                return "";
            }
        });

        scenario("reject_dlx", () -> {
            try (Connection c = connect()) {
                Channel ch = c.createChannel();
                String dlx = P + ".dlx", dlq = P + ".dlq", q = P + ".work";
                ch.exchangeDeclare(dlx, "fanout");
                ch.queueDeclare(dlq, false, false, false, null);
                ch.queueBind(dlq, dlx, "");
                ch.queueDeclare(q, false, false, false, Map.of("x-dead-letter-exchange", dlx));
                ch.basicPublish("", q, null, b("bad"));
                GetResponse m = getWait(ch, q);
                ch.basicReject(m.getEnvelope().getDeliveryTag(), false);
                GetResponse d = getWait(ch, dlq);
                @SuppressWarnings("unchecked")
                List<Map<String, Object>> deaths = (List<Map<String, Object>>) d.getProps().getHeaders().get("x-death");
                Map<String, Object> death = deaths.get(0);
                check(s(d.getBody()).equals("bad"), "body");
                check(String.valueOf(death.get("reason")).equals("rejected")
                        && String.valueOf(death.get("queue")).equals(q)
                        && ((Number) death.get("count")).longValue() == 1, "x-death " + death);
                ch.basicAck(d.getEnvelope().getDeliveryTag(), false);
                ch.queueDelete(q);
                ch.queueDelete(dlq);
                ch.exchangeDelete(dlx);
                return "";
            }
        });

        scenario("topic", () -> {
            try (Connection c = connect()) {
                Channel ch = c.createChannel();
                String x = P + ".topic", q1 = P + ".t1", q2 = P + ".t2";
                ch.exchangeDeclare(x, "topic");
                ch.queueDeclare(q1, false, false, false, null);
                ch.queueBind(q1, x, "a.*");
                ch.queueDeclare(q2, false, false, false, null);
                ch.queueBind(q2, x, "#.z");
                List<String> returned = Collections.synchronizedList(new ArrayList<>());
                ch.addReturnListener(r -> returned.add(r.getRoutingKey()));
                ch.confirmSelect();
                for (String k : List.of("a.b", "a.b.z", "x.z", "a.z", "b.c")) {
                    ch.basicPublish(x, k, true, null, b(k));
                }
                ch.waitForConfirmsOrDie(10_000);
                check(returned.equals(List.of("b.c")), "returned " + returned);
                check(count(ch, q1) == 2 && count(ch, q2) == 3, "counts " + count(ch, q1) + "," + count(ch, q2));
                ch.queueDelete(q1);
                ch.queueDelete(q2);
                ch.exchangeDelete(x);
                return "";
            }
        });

        scenario("headers", () -> {
            try (Connection c = connect()) {
                Channel ch = c.createChannel();
                String x = P + ".headers", qa = P + ".hall", qb = P + ".hany";
                ch.exchangeDeclare(x, "headers");
                ch.queueDeclare(qa, false, false, false, null);
                ch.queueDeclare(qb, false, false, false, null);
                ch.queueBind(qa, x, "", Map.of("x-match", "all", "format", "pdf", "type", "report"));
                ch.queueBind(qb, x, "", Map.of("x-match", "any", "format", "zip", "type", "log"));
                List<String> returned = Collections.synchronizedList(new ArrayList<>());
                ch.addReturnListener(r -> returned.add(s(r.getBody())));
                ch.confirmSelect();
                List<Map<String, Object>> hs = List.of(
                        Map.of("format", "pdf", "type", "report"),
                        Map.of("format", "pdf"),
                        Map.of("type", "log"),
                        Map.of("format", "zip", "type", "x"));
                for (int i = 0; i < hs.size(); i++) {
                    AMQP.BasicProperties props = new AMQP.BasicProperties.Builder().headers(hs.get(i)).build();
                    ch.basicPublish(x, "", true, props, b(String.valueOf(i)));
                }
                ch.waitForConfirmsOrDie(10_000);
                check(returned.equals(List.of("1")), "returned " + returned);
                check(count(ch, qa) == 1 && count(ch, qb) == 2, "counts " + count(ch, qa) + "," + count(ch, qb));
                ch.queueDelete(qa);
                ch.queueDelete(qb);
                ch.exchangeDelete(x);
                return "";
            }
        });

        scenario("tx", () -> {
            try (Connection c = connect()) {
                Channel ch = c.createChannel();
                String q = P + ".tx";
                ch.queueDeclare(q, false, false, false, null);
                ch.txSelect();
                ch.basicPublish("", q, null, b("rolled back"));
                ch.txRollback();
                check(count(ch, q) == 0, "rollback left a message");
                ch.basicPublish("", q, null, b("committed"));
                ch.basicPublish("", q, null, b("committed"));
                ch.txCommit();
                check(count(ch, q) == 2, "commit count " + count(ch, q));
                GetResponse m = ch.basicGet(q, false);
                ch.basicAck(m.getEnvelope().getDeliveryTag(), false);
                ch.txRollback();
                ch.close(); // the rolled-back ack leaves the message unacked, so closing requeues it
                ch = c.createChannel();
                check(count(ch, q) == 2, "ack was not rolled back");
                ch.txSelect();
                m = ch.basicGet(q, false);
                ch.basicAck(m.getEnvelope().getDeliveryTag(), false);
                ch.txCommit();
                ch.close();
                ch = c.createChannel();
                check(count(ch, q) == 1, "ack was not committed");
                ch.queueDelete(q);
                return "";
            }
        });

        scenario("prefetch", () -> {
            try (Connection c = connect()) {
                Channel ch = c.createChannel();
                String q = P + ".prefetch";
                ch.queueDeclare(q, false, false, false, null);
                ch.confirmSelect();
                for (int i = 0; i < 20; i++) {
                    ch.basicPublish("", q, null, b(String.valueOf(i)));
                }
                ch.waitForConfirmsOrDie(10_000);
                ch.basicQos(5);
                List<Long> tags = Collections.synchronizedList(new ArrayList<>());
                ch.basicConsume(q, false, new DefaultConsumer(ch) {
                    @Override
                    public void handleDelivery(String t, Envelope env, AMQP.BasicProperties p, byte[] body) {
                        tags.add(env.getDeliveryTag());
                    }
                });
                Thread.sleep(1000);
                check(tags.size() == 5, "received " + tags.size() + " with prefetch 5");
                ch.basicAck(tags.get(tags.size() - 1), true);
                Thread.sleep(1000);
                check(tags.size() == 10, "received " + tags.size() + " after acking 5");
                ch.close();
                ch = c.createChannel();
                ch.queueDelete(q);
                return "";
            }
        });

        scenario("ttl", () -> {
            try (Connection c = connect()) {
                Channel ch = c.createChannel();
                String q1 = P + ".ttlq", q2 = P + ".ttlm";
                ch.queueDeclare(q1, false, false, false, Map.of("x-message-ttl", 200));
                ch.queueDeclare(q2, false, false, false, null);
                ch.confirmSelect();
                ch.basicPublish("", q1, null, b("x"));
                ch.basicPublish("", q2, new AMQP.BasicProperties.Builder().expiration("200").build(), b("short"));
                ch.basicPublish("", q2, new AMQP.BasicProperties.Builder().expiration("60000").build(), b("long"));
                ch.waitForConfirmsOrDie(10_000);
                Thread.sleep(700);
                check(ch.basicGet(q1, true) == null, "queue TTL did not expire");
                GetResponse m = ch.basicGet(q2, true);
                check(m != null && s(m.getBody()).equals("long"), "per-message TTL: got " + (m == null ? null : s(m.getBody())));
                ch.queueDelete(q1);
                ch.queueDelete(q2);
                return "";
            }
        });

        scenario("priority", () -> {
            try (Connection c = connect()) {
                Channel ch = c.createChannel();
                String q = P + ".prio";
                ch.queueDeclare(q, false, false, false, Map.of("x-max-priority", 10));
                ch.confirmSelect();
                for (int p : new int[] {1, 9, 5, 0, 7}) {
                    ch.basicPublish("", q, new AMQP.BasicProperties.Builder().priority(p).build(), b(String.valueOf(p)));
                }
                ch.waitForConfirmsOrDie(10_000);
                List<Integer> got = new ArrayList<>();
                for (int i = 0; i < 5; i++) {
                    GetResponse r = ch.basicGet(q, true);
                    check(r != null, "get-empty after " + got);
                    got.add(Integer.parseInt(s(r.getBody())));
                }
                check(got.equals(List.of(9, 7, 5, 1, 0)), "order " + got);
                ch.queueDelete(q);
                return "";
            }
        });

        scenario("exclusive_autodelete", () -> {
            Connection a = connect();
            try (Connection bc = connect()) {
                Channel cha = a.createChannel();
                String q = cha.queueDeclare().getQueue();
                Channel chb1 = bc.createChannel();
                expectClose(() -> chb1.queueDeclarePassive(q), 405);
                a.close();
                Channel chb2 = bc.createChannel();
                expectClose(() -> chb2.queueDeclarePassive(q), 404);
                Channel chb = bc.createChannel();
                String ad = P + ".autodel";
                chb.queueDeclare(ad, false, false, true, null);
                String tag = chb.basicConsume(ad, false, new DefaultConsumer(chb));
                chb.basicCancel(tag);
                expectClose(() -> chb.queueDeclarePassive(ad), 404);
                return "";
            } finally {
                if (a.isOpen()) {
                    a.abort();
                }
            }
        });

        scenario("direct_reply_to", () -> {
            try (Connection c = connect()) {
                Channel client = c.createChannel(), server = c.createChannel();
                String q = P + ".rpc";
                server.queueDeclare(q, false, false, false, null);
                server.basicConsume(q, false, new DefaultConsumer(server) {
                    @Override
                    public void handleDelivery(String t, Envelope env, AMQP.BasicProperties p, byte[] body) throws IOException {
                        AMQP.BasicProperties reply = new AMQP.BasicProperties.Builder().correlationId(p.getCorrelationId()).build();
                        server.basicPublish("", p.getReplyTo(), reply, b(s(body).toUpperCase()));
                        server.basicAck(env.getDeliveryTag(), false);
                    }
                });
                List<String> replies = Collections.synchronizedList(new ArrayList<>());
                CountDownLatch got = new CountDownLatch(1);
                client.basicConsume("amq.rabbitmq.reply-to", true, new DefaultConsumer(client) {
                    @Override
                    public void handleDelivery(String t, Envelope env, AMQP.BasicProperties p, byte[] body) {
                        replies.add(p.getCorrelationId() + ":" + s(body));
                        got.countDown();
                    }
                });
                AMQP.BasicProperties props = new AMQP.BasicProperties.Builder()
                        .replyTo("amq.rabbitmq.reply-to").correlationId("c1").build();
                client.basicPublish("", q, props, b("ping"));
                got.await(5, TimeUnit.SECONDS);
                Thread.sleep(100);
                check(replies.equals(List.of("c1:PING")), "replies " + replies);
                server.queueDelete(q);
                return "";
            }
        });

        scenario("basic_get", () -> {
            try (Connection c = connect()) {
                Channel ch = c.createChannel();
                String q = P + ".get";
                ch.queueDeclare(q, false, false, false, null);
                ch.confirmSelect();
                for (int i = 0; i < 3; i++) {
                    ch.basicPublish("", q, null, b(String.valueOf(i)));
                }
                ch.waitForConfirmsOrDie(10_000);
                GetResponse m = ch.basicGet(q, false);
                check(m != null && m.getMessageCount() == 2 && s(m.getBody()).equals("0"),
                        "get-ok message_count " + (m == null ? null : m.getMessageCount()));
                ch.basicAck(m.getEnvelope().getDeliveryTag(), false);
                check(ch.basicGet(q, true) != null, "second get empty");
                check(ch.basicGet(q, true) != null, "third get empty");
                check(ch.basicGet(q, false) == null, "expected get-empty");
                ch.queueDelete(q);
                return "";
            }
        });

        scenario("cancel_notify", () -> {
            try (Connection c = connect()) {
                Channel ch = c.createChannel(), other = c.createChannel();
                String q = P + ".cancel";
                ch.queueDeclare(q, false, false, false, null);
                CompletableFuture<String> cancelled = new CompletableFuture<>();
                String tag = ch.basicConsume(q, false, new DefaultConsumer(ch) {
                    @Override
                    public void handleCancel(String consumerTag) {
                        cancelled.complete(consumerTag);
                    }
                });
                other.queueDelete(q);
                String got = cancelled.get(5, TimeUnit.SECONDS);
                check(got.equals(tag), "cancel notification for " + got + ", consumer " + tag);
                return "";
            }
        });

        scenario("connection_blocked", () -> {
            try (Connection c = connect()) {
                Object caps = c.getServerProperties().get("capabilities");
                check(caps instanceof Map<?, ?> m && Boolean.TRUE.equals(m.get("connection.blocked")), "capabilities " + caps);
                c.addBlockedListener(new BlockedListener() {
                    @Override
                    public void handleBlocked(String reason) {
                    }

                    @Override
                    public void handleUnblocked() {
                    }
                });
                return "";
            }
        });

        scenario("heartbeat", () -> {
            try (Connection c = connect(2)) {
                check(c.getHeartbeat() == 2, "negotiated heartbeat " + c.getHeartbeat());
                Thread.sleep(7000);
                check(c.isOpen(), "connection closed while idle: " + c.getCloseReason());
                Channel ch = c.createChannel();
                ch.queueDeclare();
                return "2s heartbeat, idle 7s";
            }
        });

        scenario("large_message", () -> {
            try (Connection c = connect()) {
                Channel ch = c.createChannel();
                String q = P + ".large";
                ch.queueDeclare(q, false, false, false, null);
                ch.confirmSelect();
                byte[] body = new byte[5 * 1024 * 1024];
                new SecureRandom().nextBytes(body);
                ch.basicPublish("", q, null, body);
                ch.waitForConfirmsOrDie(10_000);
                GetResponse m = ch.basicGet(q, true);
                check(m != null, "get-empty");
                MessageDigest sha = MessageDigest.getInstance("SHA-256");
                check(Arrays.equals(sha.digest(m.getBody()), MessageDigest.getInstance("SHA-256").digest(body)), "body mismatch");
                ch.queueDelete(q);
                return "5 MiB";
            }
        });

        scenario("throughput", () -> {
            int n = 10_000;
            try (Connection c = connect()) {
                Channel ch = c.createChannel();
                String q = P + ".tput";
                ch.queueDeclare(q, false, false, false, null);
                byte[] body = new byte[100];
                Arrays.fill(body, (byte) 'x');
                long t0 = System.nanoTime();
                for (int i = 0; i < n; i++) {
                    ch.basicPublish("", q, null, body);
                }
                double pub = (System.nanoTime() - t0) / 1e9;
                ch.basicQos(500);
                AtomicInteger got = new AtomicInteger();
                CountDownLatch done = new CountDownLatch(1);
                long t1 = System.nanoTime();
                String tag = ch.basicConsume(q, false, new DefaultConsumer(ch) {
                    @Override
                    public void handleDelivery(String t, Envelope env, AMQP.BasicProperties p, byte[] b) throws IOException {
                        int k = got.incrementAndGet();
                        if (k % 100 == 0 || k == n) {
                            getChannel().basicAck(env.getDeliveryTag(), true);
                        }
                        if (k == n) {
                            done.countDown();
                        }
                    }
                });
                check(done.await(12, TimeUnit.SECONDS), "timed out after " + got.get());
                double con = (System.nanoTime() - t1) / 1e9;
                ch.basicCancel(tag);
                ch.queueDelete(q);
                return String.format("publish %.0f msg/s, consume %.0f msg/s", n / pub, n / con);
            }
        });
    }

    static String describe(Throwable e) {
        if (e instanceof ExecutionException && e.getCause() != null) {
            e = e.getCause();
        }
        StringBuilder sb = new StringBuilder(e.getClass().getSimpleName());
        if (e.getMessage() != null) {
            sb.append(": ").append(e.getMessage());
        }
        for (Throwable c = e.getCause(); c != null && c != e; c = c.getCause()) {
            sb.append(" <- ").append(c.getClass().getSimpleName());
            if (c.getMessage() != null) {
                sb.append(": ").append(c.getMessage());
            }
            if (c.getCause() == c) {
                break;
            }
        }
        return sb.toString().replace('\n', ' ');
    }

    public static void main(String[] args) {
        Set<String> only = Set.of(args);
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
                String detail = f.get(name.equals("throughput") ? 30 : 15, TimeUnit.SECONDS);
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
        System.exit(failed > 0 ? 1 : 0);
    }
}
