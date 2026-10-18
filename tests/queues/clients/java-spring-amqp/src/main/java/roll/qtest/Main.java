package roll.qtest;

import com.rabbitmq.client.Channel;
import org.springframework.amqp.AmqpRejectAndDontRequeueException;
import org.springframework.amqp.core.AcknowledgeMode;
import org.springframework.amqp.core.Binding;
import org.springframework.amqp.core.BindingBuilder;
import org.springframework.amqp.core.Declarables;
import org.springframework.amqp.core.DirectExchange;
import org.springframework.amqp.core.ExchangeBuilder;
import org.springframework.amqp.core.FanoutExchange;
import org.springframework.amqp.core.Message;
import org.springframework.amqp.core.MessageBuilder;
import org.springframework.amqp.core.MessageProperties;
import org.springframework.amqp.core.Queue;
import org.springframework.amqp.core.QueueBuilder;
import org.springframework.amqp.core.QueueInformation;
import org.springframework.amqp.core.ReturnedMessage;
import org.springframework.amqp.core.TopicExchange;
import org.springframework.amqp.rabbit.config.SimpleRabbitListenerContainerFactory;
import org.springframework.amqp.rabbit.connection.CachingConnectionFactory;
import org.springframework.amqp.rabbit.connection.Connection;
import org.springframework.amqp.rabbit.connection.CorrelationData;
import org.springframework.amqp.rabbit.connection.RabbitConnectionFactoryBean;
import org.springframework.amqp.rabbit.core.RabbitAdmin;
import org.springframework.amqp.rabbit.core.RabbitTemplate;
import org.springframework.amqp.rabbit.listener.SimpleMessageListenerContainer;
import org.springframework.amqp.rabbit.listener.adapter.MessageListenerAdapter;
import org.springframework.amqp.rabbit.listener.api.ChannelAwareMessageListener;
import org.springframework.amqp.rabbit.transaction.RabbitTransactionManager;
import org.springframework.amqp.support.converter.JacksonJsonMessageConverter;
import org.springframework.boot.CommandLineRunner;
import org.springframework.boot.ExitCodeGenerator;
import org.springframework.boot.SpringApplication;
import org.springframework.boot.amqp.autoconfigure.CachingConnectionFactoryConfigurer;
import org.springframework.boot.amqp.autoconfigure.RabbitConnectionFactoryBeanConfigurer;
import org.springframework.boot.amqp.autoconfigure.RabbitTemplateConfigurer;
import org.springframework.boot.amqp.autoconfigure.SimpleRabbitListenerContainerFactoryConfigurer;
import org.springframework.boot.autoconfigure.SpringBootApplication;
import org.springframework.context.support.GenericApplicationContext;
import org.springframework.core.ParameterizedTypeReference;
import org.springframework.transaction.support.TransactionTemplate;
import tools.jackson.databind.json.JsonMapper;

import java.net.URI;
import java.net.URLDecoder;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.UUID;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;

/** roll queues compatibility scenarios for Spring AMQP (Spring Boot spring-boot-starter-amqp). */
@SpringBootApplication
public class Main implements CommandLineRunner, ExitCodeGenerator {
    static final String P = "spring-" + UUID.randomUUID().toString().replace("-", "").substring(0, 6);

    public record Order(String id, int qty, List<String> items) {
    }

    /** Reply handler for direct_reply_to (MessageListenerAdapter calls handleMessage). */
    public static class Upper {
        public String handleMessage(String in) {
            return in.toUpperCase();
        }
    }

    interface Scenario {
        String run(CachingConnectionFactory cf) throws Exception;
    }

    private final RabbitConnectionFactoryBeanConfigurer beanConfigurer;
    private final CachingConnectionFactoryConfigurer cfConfigurer;
    private final RabbitTemplateConfigurer templateConfigurer;
    private final SimpleRabbitListenerContainerFactoryConfigurer listenerConfigurer;
    private final JsonMapper jsonMapper;
    private final Map<String, Scenario> scenarios = new LinkedHashMap<>();
    private int exitCode;

    public Main(RabbitConnectionFactoryBeanConfigurer beanConfigurer, CachingConnectionFactoryConfigurer cfConfigurer,
            RabbitTemplateConfigurer templateConfigurer, SimpleRabbitListenerContainerFactoryConfigurer listenerConfigurer,
            JsonMapper jsonMapper) {
        this.beanConfigurer = beanConfigurer;
        this.cfConfigurer = cfConfigurer;
        this.templateConfigurer = templateConfigurer;
        this.listenerConfigurer = listenerConfigurer;
        this.jsonMapper = jsonMapper;
        register();
    }

    /** A fresh connection factory configured from spring.rabbitmq.* exactly like Boot's auto-configured one. */
    CachingConnectionFactory newCf() throws Exception {
        RabbitConnectionFactoryBean bean = new RabbitConnectionFactoryBean();
        beanConfigurer.configure(bean);
        bean.afterPropertiesSet();
        CachingConnectionFactory cf = new CachingConnectionFactory(bean.getObject());
        cfConfigurer.configure(cf);
        return cf;
    }

    RabbitTemplate template(CachingConnectionFactory cf) {
        RabbitTemplate t = new RabbitTemplate();
        templateConfigurer.configure(t, cf);
        return t;
    }

    SimpleMessageListenerContainer container(CachingConnectionFactory cf, String queue) {
        SimpleRabbitListenerContainerFactory f = new SimpleRabbitListenerContainerFactory();
        listenerConfigurer.configure(f, cf);
        SimpleMessageListenerContainer c = f.createListenerContainer();
        c.setQueueNames(queue);
        return c;
    }

    static void check(boolean cond, String msg) {
        if (!cond) {
            throw new AssertionError(msg);
        }
    }

    static long count(RabbitAdmin admin, String q) {
        QueueInformation info = admin.getQueueInfo(q);
        check(info != null, "queue " + q + " missing");
        return info.getMessageCount();
    }

    static String body(Message m) {
        return new String(m.getBody(), StandardCharsets.UTF_8);
    }

    void scenario(String name, Scenario s) {
        scenarios.put(name, s);
    }

    void register() {
        scenario("connect", cf -> {
            Connection c = cf.createConnection();
            check(c.isOpen(), "connection not open");
            Map<String, Object> props = c.getDelegate().getServerProperties();
            Channel ch = c.createChannel(false);
            check(ch.isOpen(), "channel not open");
            ch.close();
            return props.get("product") + " " + props.get("version");
        });

        scenario("declare", cf -> {
            String q1 = P + ".declare.q1", q2 = P + ".declare.q2", dx = P + ".declare.direct", tx = P + ".declare.topic";
            Queue queue1 = QueueBuilder.durable(q1).build();
            Queue queue2 = QueueBuilder.nonDurable(q2).build();
            DirectExchange direct = ExchangeBuilder.directExchange(dx).durable(true).build();
            TopicExchange topic = ExchangeBuilder.topicExchange(tx).durable(false).build();
            Declarables declarables = new Declarables(queue1, queue2, direct, topic,
                    BindingBuilder.bind(queue1).to(direct).with("a.k"),
                    BindingBuilder.bind(topic).to(direct).with("a.k"), // exchange-to-exchange
                    BindingBuilder.bind(queue2).to(topic).with("a.#"));
            try (GenericApplicationContext ctx = new GenericApplicationContext()) {
                ctx.registerBean(RabbitAdmin.class, () -> new RabbitAdmin(cf));
                ctx.registerBean("declarables", Declarables.class, () -> declarables);
                ctx.refresh();
                RabbitAdmin admin = ctx.getBean(RabbitAdmin.class);
                admin.initialize();
                check(count(admin, q1) == 0 && count(admin, q2) == 0, "fresh queues not empty");
                RabbitTemplate t = template(cf);
                t.convertAndSend(dx, "a.k", "hello");
                Message m1 = t.receive(q1, 5000), m2 = t.receive(q2, 5000);
                check(m1 != null && body(m1).equals("hello"), "direct-bound queue got " + m1);
                check(m2 != null && body(m2).equals("hello"), "exchange-to-exchange bound queue got " + m2);
                check(dx.equals(m2.getMessageProperties().getReceivedExchange()),
                        "e2e delivery exchange " + m2.getMessageProperties().getReceivedExchange());
                Queue anon = admin.declareQueue();
                check(anon != null && anon.getName().startsWith("amq.gen-"), "server-named queue " + anon);
                check(admin.deleteQueue(q1) && admin.deleteQueue(q2), "delete queues");
                check(admin.getQueueInfo(q1) == null, "queue still present after delete");
                check(admin.deleteExchange(tx) && admin.deleteExchange(dx), "delete exchanges");
                return "queues, exchanges, bindings incl. exchange-to-exchange";
            }
        });

        scenario("template_send_receive", cf -> {
            RabbitAdmin admin = new RabbitAdmin(cf);
            RabbitTemplate t = template(cf);
            String q = P + ".template";
            admin.declareQueue(new Queue(q, false));
            t.convertAndSend("", q, "hello");
            t.convertAndSend(q, "world", m -> {
                m.getMessageProperties().setHeader("x-n", 7);
                return m;
            });
            Object a = t.receiveAndConvert(q, 5000);
            check("hello".equals(a), "receiveAndConvert got " + a);
            Message m = t.receive(q, 5000);
            check(m != null && body(m).equals("world"), "receive got " + m);
            check(MessageProperties.CONTENT_TYPE_TEXT_PLAIN.equals(m.getMessageProperties().getContentType()),
                    "content type " + m.getMessageProperties().getContentType());
            Object n = m.getMessageProperties().getHeaders().get("x-n");
            check(n instanceof Number num && num.intValue() == 7, "header x-n " + n);
            check(t.receiveAndConvert(q) == null, "queue not empty");
            admin.deleteQueue(q);
            return "";
        });

        scenario("listener_ack", cf -> {
            int n = 100;
            RabbitAdmin admin = new RabbitAdmin(cf);
            RabbitTemplate t = template(cf);
            String q = P + ".listener";
            admin.declareQueue(new Queue(q, false));
            List<String> got = Collections.synchronizedList(new ArrayList<>());
            CountDownLatch done = new CountDownLatch(n);
            SimpleMessageListenerContainer c = container(cf, q);
            c.setAcknowledgeMode(AcknowledgeMode.MANUAL);
            c.setMessageListener((ChannelAwareMessageListener) (m, ch) -> {
                got.add(body(m));
                ch.basicAck(m.getMessageProperties().getDeliveryTag(), false);
                done.countDown();
            });
            c.afterPropertiesSet();
            c.start();
            try {
                // one channel for the whole batch: AMQP only orders messages published on the same channel,
                // and plain convertAndSend may rotate through several cached channels
                t.invoke(ops -> {
                    for (int i = 0; i < n; i++) {
                        ops.convertAndSend("", q, "m" + i);
                    }
                    return null;
                });
                check(done.await(10, TimeUnit.SECONDS), "timed out after " + got.size());
            } finally {
                c.stop();
            }
            List<String> want = new ArrayList<>();
            for (int i = 0; i < n; i++) {
                want.add("m" + i);
            }
            check(got.equals(want), "order " + got);
            cf.resetConnection(); // anything left unacked would be requeued now
            check(count(admin, q) == 0, "queue not empty after acks: " + count(admin, q));
            admin.deleteQueue(q);
            return n + " acked";
        });

        scenario("json_converter", cf -> {
            RabbitAdmin admin = new RabbitAdmin(cf);
            RabbitTemplate t = template(cf);
            JacksonJsonMessageConverter conv = new JacksonJsonMessageConverter(jsonMapper, "roll.qtest");
            t.setMessageConverter(conv);
            String q = P + ".json";
            admin.declareQueue(new Queue(q, false));
            Order order = new Order("o-1", 3, List.of("a", "b"));
            t.convertAndSend("", q, order);
            Message raw = t.receive(q, 5000);
            check(raw != null, "no message");
            MessageProperties mp = raw.getMessageProperties();
            check(MessageProperties.CONTENT_TYPE_JSON.equals(mp.getContentType()), "content type " + mp.getContentType());
            check(Order.class.getName().equals(mp.getHeaders().get("__TypeId__")), "__TypeId__ " + mp.getHeaders().get("__TypeId__"));
            Object back = conv.fromMessage(raw);
            check(order.equals(back), "roundtrip via __TypeId__ " + back + " json " + body(raw));
            t.convertAndSend("", q, order);
            Order typed = t.receiveAndConvert(q, 5000, new ParameterizedTypeReference<Order>() { });
            check(order.equals(typed), "roundtrip via type reference " + typed);
            admin.deleteQueue(q);
            return body(raw);
        });

        scenario("publisher_confirms", cf -> {
            check(cf.isPublisherConfirms(), "connection factory not in correlated confirm mode");
            RabbitAdmin admin = new RabbitAdmin(cf);
            RabbitTemplate t = template(cf);
            String q = P + ".confirms";
            admin.declareQueue(new Queue(q, true));
            AtomicInteger callbacks = new AtomicInteger();
            t.setConfirmCallback((cd, ack, cause) -> {
                if (ack) {
                    callbacks.incrementAndGet();
                }
            });
            List<CorrelationData> cds = new ArrayList<>();
            for (int i = 0; i < 100; i++) {
                CorrelationData cd = new CorrelationData("c" + i);
                cds.add(cd);
                t.convertAndSend("", q, "m" + i, cd);
            }
            for (CorrelationData cd : cds) {
                CorrelationData.Confirm c = cd.getFuture().get(5, TimeUnit.SECONDS);
                check(c.ack(), "nack for " + cd.getId() + ": " + c.reason());
            }
            // the futures complete just before the ConfirmCallback runs, so give the last callback a moment
            long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(2);
            while (callbacks.get() < 100 && System.nanoTime() < end) {
                Thread.sleep(10);
            }
            check(callbacks.get() == 100, "confirm callbacks " + callbacks.get());
            check(count(admin, q) == 100, "count " + count(admin, q));
            admin.deleteQueue(q);
            return "100 acked";
        });

        scenario("returns", cf -> {
            RabbitAdmin admin = new RabbitAdmin(cf);
            RabbitTemplate t = template(cf);
            String q = P + ".returns", nowhere = P + ".nowhere";
            admin.declareQueue(new Queue(q, false));
            t.setMandatory(true);
            List<ReturnedMessage> returned = Collections.synchronizedList(new ArrayList<>());
            CountDownLatch latch = new CountDownLatch(1);
            t.setReturnsCallback(r -> {
                returned.add(r);
                latch.countDown();
            });
            CorrelationData ok = new CorrelationData("routable");
            t.convertAndSend("", q, "kept", ok);
            check(ok.getFuture().get(5, TimeUnit.SECONDS).ack(), "routable message nacked");
            CorrelationData lost = new CorrelationData("unroutable");
            t.convertAndSend("", nowhere, "lost", lost);
            check(lost.getFuture().get(5, TimeUnit.SECONDS).ack(), "unroutable message nacked");
            check(latch.await(5, TimeUnit.SECONDS), "no return for unroutable mandatory message");
            Thread.sleep(100);
            check(returned.size() == 1, "returns " + returned);
            ReturnedMessage r = returned.get(0);
            check(r.getReplyCode() == 312 && "NO_ROUTE".equals(r.getReplyText()) && nowhere.equals(r.getRoutingKey())
                    && "".equals(r.getExchange()) && body(r.getMessage()).equals("lost"), "returned " + r);
            check(lost.getReturned() != null, "CorrelationData.getReturned() not set before the confirm");
            check(ok.getReturned() == null, "routable message marked returned");
            admin.deleteQueue(q);
            return "312 NO_ROUTE";
        });

        scenario("direct_reply_to", cf -> {
            RabbitAdmin admin = new RabbitAdmin(cf);
            RabbitTemplate t = template(cf);
            t.setReplyTimeout(5000);
            String q = P + ".rpc";
            admin.declareQueue(new Queue(q, false));
            AtomicReference<String> replyTo = new AtomicReference<>();
            SimpleMessageListenerContainer c = container(cf, q);
            c.setAfterReceivePostProcessors(m -> {
                replyTo.set(m.getMessageProperties().getReplyTo());
                return m;
            });
            c.setMessageListener(new MessageListenerAdapter(new Upper()));
            c.afterPropertiesSet();
            c.start();
            try {
                Object reply = t.convertSendAndReceive("", q, "ping");
                check("PING".equals(reply), "reply " + reply);
                check(replyTo.get() != null && replyTo.get().startsWith("amq.rabbitmq.reply-to"),
                        "request reply_to " + replyTo.get() + " (template fell back to a temporary reply queue)");
            } finally {
                c.stop();
                t.stop();
            }
            admin.deleteQueue(q);
            return "via amq.rabbitmq.reply-to";
        });

        scenario("transactions", cf -> {
            // a channel cannot be in tx and confirm mode at once (406), so this factory opts out of confirms/returns
            cf.setPublisherConfirmType(CachingConnectionFactory.ConfirmType.NONE);
            cf.setPublisherReturns(false);
            RabbitAdmin admin = new RabbitAdmin(cf);
            RabbitTemplate t = template(cf);
            t.setChannelTransacted(true);
            TransactionTemplate tt = new TransactionTemplate(new RabbitTransactionManager(cf));
            String q = P + ".tx";
            admin.declareQueue(new Queue(q, false));
            tt.executeWithoutResult(s -> {
                t.convertAndSend("", q, "a");
                t.convertAndSend("", q, "b");
            });
            check(count(admin, q) == 2, "commit count " + count(admin, q));
            tt.executeWithoutResult(s -> {
                t.convertAndSend("", q, "x");
                s.setRollbackOnly();
            });
            check(count(admin, q) == 2, "rollback-only left a message: " + count(admin, q));
            try {
                tt.executeWithoutResult(s -> {
                    t.convertAndSend("", q, "y");
                    throw new IllegalStateException("boom");
                });
                throw new AssertionError("exception was swallowed");
            } catch (IllegalStateException expected) {
                // rolled back
            }
            check(count(admin, q) == 2, "exception rollback left a message: " + count(admin, q));
            tt.executeWithoutResult(s -> {
                check("a".equals(t.receiveAndConvert(q)), "receive in tx");
                s.setRollbackOnly();
            });
            check(count(admin, q) == 2, "rolled-back receive was not requeued: " + count(admin, q));
            tt.executeWithoutResult(s -> check(t.receiveAndConvert(q) != null, "receive in tx"));
            check(count(admin, q) == 1, "committed receive count " + count(admin, q));
            admin.deleteQueue(q);
            return "commit, rollback-only, exception rollback, receive rollback/commit";
        });

        scenario("dlx", cf -> {
            RabbitAdmin admin = new RabbitAdmin(cf);
            RabbitTemplate t = template(cf);
            String dlx = P + ".dlx", dlq = P + ".dlq", q = P + ".work";
            FanoutExchange x = new FanoutExchange(dlx, false, false);
            Queue dead = new Queue(dlq, false);
            admin.declareExchange(x);
            admin.declareQueue(dead);
            admin.declareBinding(BindingBuilder.bind(dead).to(x));
            admin.declareQueue(QueueBuilder.nonDurable(q).deadLetterExchange(dlx).build());
            AtomicInteger attempts = new AtomicInteger();
            SimpleMessageListenerContainer c = container(cf, q);
            c.setMessageListener(m -> {
                attempts.incrementAndGet();
                throw new AmqpRejectAndDontRequeueException("bad message");
            });
            c.afterPropertiesSet();
            c.start();
            Message d;
            try {
                t.convertAndSend("", q, "bad");
                d = t.receive(dlq, 5000);
            } finally {
                c.stop();
            }
            check(d != null && body(d).equals("bad"), "dead-lettered message " + d);
            List<Map<String, ?>> deaths = d.getMessageProperties().getXDeathHeader();
            check(deaths != null && !deaths.isEmpty(), "no x-death header: " + d.getMessageProperties().getHeaders());
            Map<String, ?> death = deaths.get(0);
            check(String.valueOf(death.get("reason")).equals("rejected") && String.valueOf(death.get("queue")).equals(q)
                    && ((Number) death.get("count")).longValue() == 1, "x-death " + death);
            check(attempts.get() == 1, "listener attempts " + attempts.get());
            admin.deleteQueue(q);
            admin.deleteQueue(dlq);
            admin.deleteExchange(dlx);
            return "";
        });

        scenario("prefetch", cf -> {
            RabbitAdmin admin = new RabbitAdmin(cf);
            RabbitTemplate t = template(cf);
            String q = P + ".prefetch";
            admin.declareQueue(new Queue(q, false));
            for (int i = 0; i < 20; i++) {
                t.convertAndSend("", q, String.valueOf(i));
            }
            long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
            while (count(admin, q) < 20 && System.nanoTime() < end) {
                Thread.sleep(20);
            }
            List<Long> tags = Collections.synchronizedList(new ArrayList<>());
            AtomicReference<Channel> channel = new AtomicReference<>();
            SimpleMessageListenerContainer c = container(cf, q);
            c.setPrefetchCount(5);
            c.setAcknowledgeMode(AcknowledgeMode.MANUAL);
            c.setMessageListener((ChannelAwareMessageListener) (m, ch) -> {
                channel.set(ch);
                tags.add(m.getMessageProperties().getDeliveryTag());
            });
            c.afterPropertiesSet();
            c.start();
            try {
                Thread.sleep(1000);
                check(tags.size() == 5, "received " + tags.size() + " with prefetch 5");
                channel.get().basicAck(tags.get(tags.size() - 1), true);
                Thread.sleep(1000);
                check(tags.size() == 10, "received " + tags.size() + " after acking 5");
            } finally {
                c.stop();
            }
            cf.resetConnection();
            admin.deleteQueue(q);
            return "";
        });

        scenario("throughput", cf -> {
            int n = 10_000;
            RabbitAdmin admin = new RabbitAdmin(cf);
            RabbitTemplate t = template(cf);
            String q = P + ".tput";
            admin.declareQueue(new Queue(q, false));
            Message msg = MessageBuilder.withBody("x".repeat(100).getBytes(StandardCharsets.UTF_8)).build();
            long t0 = System.nanoTime();
            t.invoke(ops -> {
                for (int i = 0; i < n; i++) {
                    ops.send("", q, msg);
                }
                return null;
            });
            double pub = (System.nanoTime() - t0) / 1e9;
            AtomicInteger got = new AtomicInteger();
            CountDownLatch done = new CountDownLatch(n);
            SimpleMessageListenerContainer c = container(cf, q);
            c.setPrefetchCount(500);
            c.setBatchSize(100);
            c.setMessageListener(m -> {
                got.incrementAndGet();
                done.countDown();
            });
            c.afterPropertiesSet();
            long t1 = System.nanoTime();
            c.start();
            double con;
            try {
                check(done.await(20, TimeUnit.SECONDS), "timed out after " + got.get());
                con = (System.nanoTime() - t1) / 1e9;
            } finally {
                c.stop();
            }
            cf.resetConnection();
            check(count(admin, q) == 0, "queue not drained: " + count(admin, q));
            admin.deleteQueue(q);
            return String.format("publish %.0f msg/s, consume %.0f msg/s", n / pub, n / con);
        });
    }

    static String describe(Throwable e) {
        StringBuilder sb = new StringBuilder();
        Set<Throwable> seen = Collections.newSetFromMap(new java.util.IdentityHashMap<>());
        for (Throwable c = e; c != null && seen.add(c); c = c.getCause()) {
            if (sb.length() > 0) {
                sb.append(" <- ");
            }
            sb.append(c.getClass().getSimpleName());
            if (c.getMessage() != null) {
                sb.append(": ").append(c.getMessage());
            }
        }
        return sb.toString().replace('\n', ' ');
    }

    @Override
    public void run(String... args) {
        Set<String> only = Set.of(args);
        for (String a : args) {
            if (!scenarios.containsKey(a)) {
                System.out.println("FAIL " + a + ": unknown scenario");
                exitCode = 1;
            }
        }
        ExecutorService pool = Executors.newCachedThreadPool(r -> {
            Thread th = new Thread(r);
            th.setDaemon(true);
            return th;
        });
        for (Map.Entry<String, Scenario> e : scenarios.entrySet()) {
            String name = e.getKey();
            if (!only.isEmpty() && !only.contains(name)) {
                continue;
            }
            long t = System.nanoTime();
            Future<String> f = pool.submit(() -> {
                CachingConnectionFactory cf = newCf();
                try {
                    return e.getValue().run(cf);
                } finally {
                    cf.destroy();
                }
            });
            try {
                String detail = f.get(name.equals("throughput") ? 30 : 15, TimeUnit.SECONDS);
                String secs = String.format("(%.1fs)", (System.nanoTime() - t) / 1e9);
                System.out.println(("PASS " + name + " " + detail + " " + secs).replace("  ", " "));
            } catch (TimeoutException te) {
                exitCode = 1;
                f.cancel(true);
                System.out.println("FAIL " + name + ": scenario timed out");
            } catch (Throwable ex) {
                exitCode = 1;
                Throwable cause = ex instanceof ExecutionException && ex.getCause() != null ? ex.getCause() : ex;
                System.out.println("FAIL " + name + ": " + describe(cause));
                cause.printStackTrace(System.err);
            }
            System.out.flush();
        }
    }

    @Override
    public int getExitCode() {
        return exitCode;
    }

    /** Maps AMQP_URL (amqp://user:pass@host:port/vhost, vhost percent-encoded) onto spring.rabbitmq.*. */
    static Map<String, Object> brokerProperties() {
        URI u = URI.create(System.getenv().getOrDefault("AMQP_URL", "amqp://guest:guest@localhost:5672/%2F"));
        Map<String, Object> p = new HashMap<>();
        p.put("spring.rabbitmq.host", u.getHost());
        p.put("spring.rabbitmq.port", u.getPort() > 0 ? u.getPort() : 5672);
        if (u.getRawUserInfo() != null) {
            String[] ui = u.getRawUserInfo().split(":", 2);
            p.put("spring.rabbitmq.username", URLDecoder.decode(ui[0], StandardCharsets.UTF_8));
            if (ui.length > 1) {
                p.put("spring.rabbitmq.password", URLDecoder.decode(ui[1], StandardCharsets.UTF_8));
            }
        }
        String path = u.getRawPath() == null || u.getRawPath().length() <= 1 ? "" : u.getRawPath().substring(1);
        p.put("spring.rabbitmq.virtual-host", path.isEmpty() ? "/" : URLDecoder.decode(path, StandardCharsets.UTF_8));
        return p;
    }

    public static void main(String[] args) {
        SpringApplication app = new SpringApplication(Main.class);
        app.setDefaultProperties(brokerProperties());
        System.exit(SpringApplication.exit(app.run(args)));
    }
}
