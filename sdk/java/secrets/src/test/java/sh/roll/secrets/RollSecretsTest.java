package sh.roll.secrets;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.charset.StandardCharsets;
import java.time.Clock;
import java.time.Duration;
import java.time.Instant;
import java.time.ZoneOffset;
import java.util.ArrayList;
import java.util.Base64;
import java.util.List;
import java.util.Map;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import org.junit.jupiter.api.Test;

class RollSecretsTest {
    // A box sealed by @roll/secrets' sealTo (the same box as the server's seal_to), with a fixed
    // key: the same vector as sdk/go/secrets and sdk/python/roll_secrets.
    static final String VECTOR_KEY = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA";
    static final String VECTOR_CONTEXT = "roll-secrets:prj_a:production:app:web";
    static final Sealed VECTOR_BOX = new Sealed(1, "aAGjVaESpknkSH_uG48K1a_unnp97Gh4uRwOYWrjHkk", "yct4_uAUdnUKxskR",
            "-UbR7dzvRmoXgBHnbyoW00qyGY07tIB_0vx6VrXQhPPRNns7PU3irIJq5M5Xjto");

    static SealKey vectorKey() {
        return SealKey.fromPrivateBytes(Base64.getUrlDecoder().decode(VECTOR_KEY));
    }

    static final String ENV = "/v1/secrets/projects/prj_a/environments/production";

    @Test
    void aBoxSealedByRollOpensOnlyForItsContext() {
        SealKey k = vectorKey();
        assertEquals("{\"DB_URL\":\"postgres://db/shop\"}", new String(k.open(VECTOR_BOX, VECTOR_CONTEXT), StandardCharsets.UTF_8));
        assertThrows(IllegalArgumentException.class, () -> k.open(VECTOR_BOX, "roll-secrets:prj_a:staging:app:web"));
        assertThrows(IllegalArgumentException.class, () -> SealKey.generate().open(VECTOR_BOX, VECTOR_CONTEXT));
        assertEquals(43, SealKey.generate().publicKey().length());
    }

    /**
     * A workload identity's login, a sealed resolve, a watch whose long poll brings a change at
     * once, and a refused roll token that the client replaces with a new login.
     */
    @Test
    void aWorkloadLogsInReadsSealedValuesAndWatchesForChanges() throws Exception {
        Object lock = new Object();
        String[] state = {"d1", "one", "rlt_workload"}; // digest, value, the token that works
        AtomicInteger logins = new AtomicInteger();
        try (FakeRolld rolld = new FakeRolld(r -> {
            String auth = r.header("Authorization");
            if (r.path.equals("/v1/secrets/login")) {
                Map<String, Object> b = r.json();
                if (!"idn_ci".equals(b.get("identity")) || !"the-oidc-token".equals(b.get("token"))) {
                    r.answer(401, Map.of("error", "no identity idn_ci takes this token", "code", "unauthorized"));
                    return;
                }
                logins.incrementAndGet();
                synchronized (lock) {
                    r.answer(200, Map.of("token", state[2], "project", "prj_a", "expires_at", System.currentTimeMillis() + 3_600_000L));
                }
                return;
            }
            synchronized (lock) {
                if (!("Bearer " + state[2]).equals(auth)) {
                    r.answer(401, Map.of("error", "unauthorized"));
                    return;
                }
            }
            switch (r.path) {
                case ENV + "/digest": {
                    String since = r.query.get("since");
                    long until = System.currentTimeMillis() + 1000L * Long.parseLong(r.query.getOrDefault("wait", "0"));
                    synchronized (lock) {
                        // A long poll: held until the digest moves or the wait runs out.
                        while (since != null && since.equals(state[0]) && System.currentTimeMillis() < until) {
                            lock.wait(Math.max(1, until - System.currentTimeMillis()));
                        }
                        r.answer(200, Map.of("digest", state[0]));
                    }
                    return;
                }
                case ENV + "/values":
                    synchronized (lock) {
                        r.answer(200, FakeRolld.values(Map.of("A", state[1]), List.of()));
                    }
                    return;
                case ENV + "/resolve": {
                    Map<String, Object> b = r.json();
                    if (!"app:web".equals(b.get("consumer")) || !vectorKey().publicKey().equals(b.get("seal_to"))) {
                        r.answer(400, Map.of("error", "bad resolve"));
                        return;
                    }
                    r.answer(200, Map.of("sealed", Map.of("v", 1, "epk", VECTOR_BOX.epk(), "nonce", VECTOR_BOX.nonce(), "ct", VECTOR_BOX.ct()),
                            "context", VECTOR_CONTEXT, "problems", List.of(), "leases", List.of(), "dynamic", List.of(), "linked", List.of("DB_URL")));
                    return;
                }
                default:
                    r.answer(404, Map.of("error", "not found"));
            }
        })) {
            Map<String, String> env = new java.util.HashMap<>(Map.of("ROLL_API_URL", rolld.url(), "ROLL_IDENTITY", "idn_ci", "ROLL_OIDC_TOKEN", "wrong"));
            RollSecretsException refused = assertThrows(RollSecretsException.class,
                    () -> RollSecrets.builder(env::get).build().values("production"));
            assertEquals(401, refused.status());
            assertEquals("unauthorized", refused.code());

            env.put("ROLL_OIDC_TOKEN", "the-oidc-token");
            RollSecrets c = RollSecrets.builder(env::get).build();
            Values sealed = c.resolveSealed("production", "", "app:web", vectorKey());
            assertEquals("postgres://db/shop", sealed.get("DB_URL"));
            assertEquals(List.of("DB_URL"), sealed.linked());

            BlockingQueue<String> seen = new LinkedBlockingQueue<>();
            List<RuntimeException> errors = new ArrayList<>();
            // `every` is long: only the long poll can bring a change in time.
            try (Watch w = c.watch("production", WatchOptions.of(v -> seen.add(v.get("A"))).every(Duration.ofSeconds(30)).onError(errors::add))) {
                assertEquals("one", seen.poll(5, TimeUnit.SECONDS));
                Thread.sleep(100); // the long poll is held now
                long changed = System.nanoTime();
                synchronized (lock) {
                    state[0] = "d2";
                    state[1] = "two";
                    lock.notifyAll();
                }
                assertEquals("two", seen.poll(5, TimeUnit.SECONDS));
                assertTrue(System.nanoTime() - changed < 2_000_000_000L, "the long poll answered as the digest moved");

                // The roll token stops working (revoked, ended): the client logs in again and keeps watching.
                synchronized (lock) {
                    state[0] = "d3";
                    state[1] = "three";
                    state[2] = "rlt_workload2";
                    lock.notifyAll();
                }
                assertEquals("three", seen.poll(5, TimeUnit.SECONDS));
                assertEquals(2, logins.get());
                assertNull(seen.poll(200, TimeUnit.MILLISECONDS), "an unchanged digest delivered again");
                assertTrue(w.running());
            }
            // Stopped: a change reaches nobody.
            synchronized (lock) {
                state[0] = "d4";
                state[1] = "four";
                lock.notifyAll();
            }
            assertNull(seen.poll(300, TimeUnit.MILLISECONDS));
        }
    }

    /** A server without long polls answers at once: the watch asks again every `every`. */
    @Test
    void aWatchPollsAServerThatAnswersAtOnce() throws Exception {
        AtomicInteger digest = new AtomicInteger(1);
        AtomicInteger digests = new AtomicInteger();
        try (FakeRolld rolld = new FakeRolld(r -> {
            if (r.path.equals(ENV + "/digest")) {
                digests.incrementAndGet();
                r.answer(200, Map.of("digest", "d" + digest.get()));
            } else if (r.path.equals(ENV + "/values")) {
                if (digest.get() == 2) {
                    r.answer(200, Map.of("values", Map.of(), "problems", List.of(Map.of("key", "A", "problem", "no such key B")), "leases", List.of()));
                } else {
                    r.answer(200, FakeRolld.values(Map.of("A", "v" + digest.get()), List.of()));
                }
            } else {
                r.answer(404, Map.of("error", "not found"));
            }
        })) {
            RollSecrets c = RollSecrets.builder(k -> null).api(rolld.url()).token("rlt_fake").project("prj_a").build();
            BlockingQueue<String> seen = new LinkedBlockingQueue<>();
            BlockingQueue<RuntimeException> errors = new LinkedBlockingQueue<>();
            try (Watch w = c.watch("production", WatchOptions.of(v -> seen.add(v.get("A"))).every(Duration.ofMillis(20)).onError(errors::add))) {
                assertEquals("v1", seen.poll(5, TimeUnit.SECONDS));
                // A change that doesn't resolve keeps what's running and says why.
                digest.set(2);
                RollSecretsException e = (RollSecretsException) errors.poll(5, TimeUnit.SECONDS);
                assertEquals(409, e.status());
                assertEquals("unresolved", e.code());
                digest.set(3);
                assertEquals("v3", seen.poll(5, TimeUnit.SECONDS));
                int before = digests.get();
                Thread.sleep(200);
                int asked = digests.get() - before;
                assertTrue(asked >= 2 && asked <= 15, "paced by `every`, not spinning: " + asked);
                assertTrue(w.running());
            }
            assertTrue(rolld.seen.stream().anyMatch(s -> s.contains("since=d1&wait=25")), "it asked for a long poll");
        }
    }

    /**
     * Values are kept for cacheTtl, revalidated with their digest (a 304 keeps them), and never
     * kept when they hold dynamic credentials.
     */
    @Test
    void valuesAreCachedAndRevalidatedWithTheirDigest() throws Exception {
        String[] state = {"d1", "1"};
        List<String> dynamic = new ArrayList<>();
        List<String> asked = new ArrayList<>();
        try (FakeRolld rolld = new FakeRolld(r -> {
            String tag = r.header("If-None-Match");
            synchronized (asked) {
                asked.add(String.valueOf(tag));
            }
            assertEquals("1", r.query.get("digest"));
            r.header("ETag", "\"" + state[0] + "\"");
            if (("\"" + state[0] + "\"").equals(tag)) {
                r.answer(304, null);
                return;
            }
            r.answer(200, FakeRolld.values(Map.of("A", state[1]), List.copyOf(dynamic)));
        })) {
            AtomicLong now = new AtomicLong();
            Clock clock = new Clock() {
                public java.time.ZoneId getZone() { return ZoneOffset.UTC; }
                public Clock withZone(java.time.ZoneId z) { return this; }
                public Instant instant() { return Instant.ofEpochMilli(now.get()); }
            };
            RollSecrets c = RollSecrets.builder(k -> null).api(rolld.url()).token("rlt_fake").project("prj_a")
                    .cacheTtl(Duration.ofSeconds(20)).clock(clock).build();
            assertEquals("1", c.values("production").get("A"));
            assertEquals("1", c.values("production").get("A"));
            assertEquals(List.of("null"), asked, "within the TTL, from memory");

            now.addAndGet(25_000);
            assertEquals("1", c.values("production").get("A"));
            assertEquals(List.of("null", "\"d1\""), asked, "revalidated: a 304");

            now.addAndGet(25_000);
            state[0] = "d2";
            state[1] = "2";
            dynamic.add("PW");
            assertEquals("2", c.values("production").get("A"));
            assertEquals("2", c.values("production").get("A"));
            assertEquals(4, asked.size(), "a change, then dynamic credentials read every time");
        }
    }

    /** One key, errors with roll's code, and nothing printable that holds a value. */
    @Test
    void getReadsOneKeyAndNothingPrintsAValue() throws Exception {
        try (FakeRolld rolld = new FakeRolld(r -> {
            switch (r.path) {
                case "/v1/secrets/whoami":
                    r.answer(200, Map.of("project", "prj_a"));
                    return;
                case ENV + "/secrets/API_KEY/value":
                    assertEquals("/svc", r.query.get("path"));
                    r.answer(200, Map.of("value", "fake-value-123", "problems", List.of()));
                    return;
                case ENV + "/secrets/BROKEN/value":
                    r.answer(200, Map.of("value", "", "problems", List.of(Map.of("key", "BROKEN", "problem", "a reference to nothing"))));
                    return;
                case ENV + "/values":
                    r.answer(200, FakeRolld.values(Map.of("API_KEY", "fake-value-123"), List.of()));
                    return;
                default:
                    r.answer(403, Map.of("error", "this token can't read production", "code", "forbidden"));
            }
        })) {
            RollSecrets c = RollSecrets.builder(k -> null).api(rolld.url() + "/").token("rlt_fake").build();
            assertEquals("fake-value-123", c.get("production", "/svc", "API_KEY"));
            assertEquals("prj_a", c.projectId());
            RollSecretsException unresolved = assertThrows(RollSecretsException.class, () -> c.get("production", "", "BROKEN"));
            assertEquals("unresolved", unresolved.code());
            RollSecretsException forbidden = assertThrows(RollSecretsException.class, () -> c.digest("staging", ""));
            assertEquals(403, forbidden.status());
            assertEquals("forbidden", forbidden.code());
            assertEquals("this token can't read production", forbidden.getMessage());

            Values v = c.values("production");
            assertEquals("fake-value-123", v.get("API_KEY"));
            assertTrue(v.toString().contains("API_KEY"));
            assertFalse(v.toString().contains("fake-value-123"));
            assertFalse(c.toString().contains("rlt_fake"));
            assertThrows(UnsupportedOperationException.class, () -> v.values().put("B", "x"));
        }
    }

    @Test
    void jsonRoundTripsTheShapesTheClientUses() {
        Map<String, Object> o = Json.object("{\"a\":\"\\u00e9\\n\\\"x\\\"\",\"b\":[1,2.5,true,null],\"c\":{}}");
        assertEquals("é\n\"x\"", o.get("a"));
        assertEquals(java.util.Arrays.asList(1L, 2.5, true, null), o.get("b"));
        assertEquals(o, Json.object(Json.write(o)));
        RuntimeException bad = assertThrows(IllegalArgumentException.class, () -> Json.object("{\"k\":\"fake-value-123\""));
        assertFalse(bad.getMessage().contains("fake-value-123"));
    }
}
