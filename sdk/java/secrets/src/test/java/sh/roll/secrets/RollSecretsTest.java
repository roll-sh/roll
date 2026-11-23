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
        // The public key seal_to sends, as Node's own X25519 derives it from the vector's key.
        assertEquals("B6N8vBQgk8i3VdwbEOhstCY3StFqqFPtC9_AsrhtHHw", k.publicKey());
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
     * Values are kept for cacheTtl, revalidated with their digest (a 304 keeps them; here a rolld
     * without delta reads), and never kept when they hold dynamic credentials.
     */
    @Test
    void valuesAreCachedAndRevalidatedWithTheirDigest() throws Exception {
        String[] state = {"d1", "1"};
        List<String> dynamic = new ArrayList<>();
        List<String> asked = new ArrayList<>();
        try (FakeRolld rolld = new FakeRolld(r -> {
            if (r.path.endsWith("/values/delta")) {
                r.answer(404, Map.of("error", "no route"));
                return;
            }
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

    /**
     * After a one-key change a watch reads only what changed ({@code …/values/delta}: each key's
     * stamp is its version, the basis fixed) and hands on the whole folder; an older rolld (a 404
     * on the route) is read in full.
     */
    @Test
    void aWatchReadsAChangeThroughTheDeltaAndAnOlderRolldInFull() throws Exception {
        Map<String, String> values = new java.util.TreeMap<>(Map.of("A", "1", "B", "2"));
        Map<String, Integer> versions = new java.util.TreeMap<>(Map.of("A", 1, "B", 1));
        boolean[] route = {true};
        List<List<String>> deltas = new ArrayList<>();
        AtomicInteger full = new AtomicInteger();
        try (FakeRolld rolld = new FakeRolld(r -> {
            synchronized (values) {
                StringBuilder digest = new StringBuilder("d");
                versions.forEach((k, v) -> digest.append(k).append(v));
                if (r.path.equals(ENV + "/digest")) {
                    r.answer(200, Map.of("digest", digest.toString()));
                } else if (r.path.equals(ENV + "/values")) {
                    full.incrementAndGet();
                    r.answer(200, FakeRolld.values(new java.util.LinkedHashMap<>(values), List.of()));
                } else if (r.path.equals(ENV + "/values/delta") && route[0]) {
                    Map<String, Object> claim = r.json();
                    boolean delta = "b1".equals(claim.get("basis"));
                    Map<String, Object> held = Json.obj(claim, "stamps") == null ? Map.of() : Json.obj(claim, "stamps");
                    Map<String, Object> changed = new java.util.LinkedHashMap<>();
                    Map<String, Object> stamps = new java.util.LinkedHashMap<>();
                    values.forEach((k, v) -> {
                        String stamp = String.valueOf(versions.get(k));
                        if (!delta || !stamp.equals(held.get(k))) {
                            changed.put(k, v);
                            stamps.put(k, stamp);
                        }
                    });
                    deltas.add(new ArrayList<>(changed.keySet()));
                    Map<String, Object> a = FakeRolld.values(changed, List.of());
                    a.put("delta", delta);
                    a.put("digest", digest.toString());
                    a.put("basis", "b1");
                    a.put("removed", List.of());
                    a.put("stamps", stamps);
                    r.answer(200, a);
                } else {
                    r.answer(404, Map.of("error", "not found"));
                }
            }
        })) {
            RollSecrets c = RollSecrets.builder(k -> null).api(rolld.url()).token("rlt_fake").project("prj_a").build();
            BlockingQueue<Map<String, String>> seen = new LinkedBlockingQueue<>();
            try (Watch w = c.watch("production", WatchOptions.of(v -> seen.add(v.values())).every(Duration.ofMillis(20)))) {
                assertEquals(Map.of("A", "1", "B", "2"), seen.poll(5, TimeUnit.SECONDS));
                synchronized (values) {
                    values.put("B", "3");
                    versions.put("B", 2);
                }
                assertEquals(Map.of("A", "1", "B", "3"), seen.poll(5, TimeUnit.SECONDS));
                synchronized (values) {
                    assertEquals(List.of(List.of("A", "B"), List.of("B")), deltas, "only B came back, merged into what the watch held");
                    assertEquals(0, full.get());
                    route[0] = false;
                    values.put("A", "9");
                    versions.put("A", 2);
                }
                assertEquals(Map.of("A", "9", "B", "3"), seen.poll(5, TimeUnit.SECONDS));
                assertEquals(1, full.get(), "a 404 reads in full");
                assertTrue(w.running());
            }
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

    /**
     * Transit sends bytes as base64 and decrypts back to the same bytes, as the other clients do;
     * a context is bound in, a refused key is roll's error, and nothing printable holds plaintext,
     * an input or the token.
     */
    @Test
    void transitRoundTripsBytesUnderItsContextAndPrintsNoPlaintext() throws Exception {
        String keys = "/v1/secrets/projects/prj_a/transit/";
        List<Map<String, Object>> sent = new ArrayList<>();
        try (FakeRolld rolld = new FakeRolld(r -> {
            Map<String, Object> b = r.body.isEmpty() ? Map.of() : r.json();
            synchronized (sent) {
                sent.add(b);
            }
            Object context = b.getOrDefault("context", "");
            // A server that "encrypts" by echoing with the context alongside: enough to check the
            // encoding both ways and that the context goes with both calls.
            switch (r.path) {
                case "/v1/secrets/projects/prj_a/transit/cards/encrypt":
                    r.answer(200, Map.of("ciphertext", "roll:v1:" + b.get("plaintext") + ":" + context));
                    return;
                case "/v1/secrets/projects/prj_a/transit/cards/decrypt": {
                    String[] ct = ((String) b.get("ciphertext")).split(":", 4);
                    if (!ct[3].equals(context)) {
                        r.answer(400, Map.of("error", "the ciphertext doesn't open under this key and context"));
                        return;
                    }
                    r.answer(200, Map.of("plaintext", ct[2]));
                    return;
                }
                case "/v1/secrets/projects/prj_a/transit/cards/sign":
                    r.answer(200, Map.of("signature", "roll:v2:sig-" + b.get("input")));
                    return;
                case "/v1/secrets/projects/prj_a/transit/cards/verify":
                    r.answer(200, Map.of("valid", ("roll:v2:sig-" + b.get("input")).equals(b.get("signature"))));
                    return;
                case "/v1/secrets/projects/prj_a/transit/cards/hmac":
                    r.answer(200, Map.of("hmac", "roll:v2:hmac-" + b.get("input")));
                    return;
                case "/v1/secrets/projects/prj_a/transit/cards/public":
                    r.answer(200, List.of(
                            Map.of("version", 1, "public_key", "ZmFrZS1wdWJsaWMta2V5LTE=", "pem", "-----BEGIN PUBLIC KEY-----\nfake1\n-----END PUBLIC KEY-----\n",
                                    "jwk", Map.of("kty", "OKP", "crv", "Ed25519", "x", "ZmFrZS0x", "kid", "cards:v1", "alg", "EdDSA", "use", "sig")),
                            Map.of("version", 2, "public_key", "ZmFrZS1wdWJsaWMta2V5LTI=", "pem", "-----BEGIN PUBLIC KEY-----\nfake2\n-----END PUBLIC KEY-----\n",
                                    "jwk", Map.of("kty", "OKP", "crv", "Ed25519", "x", "ZmFrZS0y", "kid", "cards:v2", "alg", "EdDSA", "use", "sig"))));
                    return;
                default:
                    r.answer(403, Map.of("error", "this token can't use the key payroll", "code", "forbidden"));
            }
        })) {
            RollSecrets c = RollSecrets.builder(k -> null).api(rolld.url()).token("rlt_fake_transit").project("prj_a").build();
            Transit cards = c.transit("cards");

            String ct = cards.encrypt("héllo ✓", "order:42");
            assertEquals(Map.of("plaintext", "aMOpbGxvIOKckw==", "context", "order:42"), sent.get(0));
            assertEquals("héllo ✓", new String(cards.decrypt(ct, "order:42"), StandardCharsets.UTF_8));
            RollSecretsException elsewhere = assertThrows(RollSecretsException.class, () -> cards.decrypt(ct, "order:43"));
            assertEquals(400, elsewhere.status());

            byte[] raw = {0, (byte) 255, (byte) 128, 7};
            String bare = cards.encrypt(raw);
            assertFalse(sent.get(sent.size() - 1).containsKey("context"), "no context isn't sent");
            assertTrue(java.util.Arrays.equals(raw, cards.decrypt(bare)));

            String sig = cards.sign("fake-webhook-body");
            assertTrue(sig.startsWith("roll:v2:"));
            assertTrue(cards.verify("fake-webhook-body", sig));
            assertFalse(cards.verify("fake-webhook-body-changed", sig));
            assertEquals(cards.hmac("fake@example.com"), cards.hmac("fake@example.com".getBytes(StandardCharsets.UTF_8)));

            List<TransitPublicKey> pub = cards.publicKeys();
            assertEquals(List.of(1, 2), pub.stream().map(TransitPublicKey::version).collect(java.util.stream.Collectors.toList()));
            assertEquals("cards:v2", pub.get(1).jwk().get("kid"));
            assertEquals("ZmFrZS1wdWJsaWMta2V5LTI=", pub.get(1).publicKey());

            String plaintext = "fake-card-4242-4242";
            String encoded = Base64.getEncoder().encodeToString(plaintext.getBytes(StandardCharsets.UTF_8));
            Transit payroll = c.transit("payroll");
            RollSecretsException refused = assertThrows(RollSecretsException.class, () -> payroll.encrypt(plaintext, "order:42"));
            assertEquals(403, refused.status());
            assertEquals("forbidden", refused.code());
            assertEquals("this token can't use the key payroll", refused.getMessage());
            for (RuntimeException e : List.of(refused,
                    assertThrows(RollSecretsException.class, () -> payroll.sign(plaintext)),
                    assertThrows(RollSecretsException.class, () -> payroll.decrypt("roll:v1:" + encoded + ":")))) {
                java.io.StringWriter trace = new java.io.StringWriter();
                e.printStackTrace(new java.io.PrintWriter(trace));
                for (String secret : List.of(plaintext, encoded, "rlt_fake_transit")) {
                    assertFalse(trace.toString().contains(secret), "an error shows " + secret);
                }
            }
            for (Object printed : List.of(cards, payroll, c)) {
                assertFalse(printed.toString().contains("rlt_fake_transit"));
            }
            assertEquals("Transit[cards]", cards.toString());
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
