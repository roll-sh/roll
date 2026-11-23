package sh.roll.secrets;

import java.io.IOException;
import java.io.InputStream;
import java.net.URI;
import java.net.URLEncoder;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Clock;
import java.time.Duration;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.ConcurrentHashMap;
import java.util.function.Function;
import java.util.function.Supplier;

/**
 * A client for roll secrets (docs/secrets.md, "Consumption"): an environment's values, the runtime
 * resolve (sealed to a key only this process holds, if asked), the digest that changes when the
 * values would, and a watch that follows it. It mirrors {@code @roll/secrets} and
 * {@code sdk/go/secrets}, with the JDK only.
 *
 * <p>A client holds a roll token ({@code rlt_…}) with a rule on one project, the operator token,
 * or the short-lived token a workload identity's login gives ({@link Builder#identity}). It's safe
 * to share between threads.
 */
public final class RollSecrets {
    /** The most of an answer that's read: past it, the answer is refused. */
    private static final int MAX_BODY = 16 << 20;
    /** The server holds a long poll at most this long. */
    private static final Duration MAX_WAIT = Duration.ofSeconds(25);

    private final String api;
    private final HttpClient http;
    private final Duration timeout;
    private final Duration cacheTtl;
    private final Clock clock;
    private final Relogin login;
    private volatile String token;
    private volatile String project;

    private final Map<String, Cached> cache = new ConcurrentHashMap<>();
    /** A rolld without {@code …/values/delta}: reads are full (conditional, with cacheTtl). */
    private volatile boolean noDelta;

    /** A Values answer kept by cacheTtl, with its digest and stamps, and when it was asked. */
    private record Cached(Held held, long at) { }

    /**
     * A folder's last answer with every key's stamp, so a read after a change asks only for what
     * changed ({@code …/values/delta}, docs/secrets.md "Delta reads"): what a watch keeps, and what
     * cacheTtl keeps. {@code off}: the folder's claim is past what rolld takes (a 413); its reads
     * are full.
     */
    static final class Held {
        Values values;
        String digest = "";
        String basis = "";
        Map<String, String> stamps = Map.of();
        boolean off;

        Held copy() {
            Held h = new Held();
            h.values = values;
            h.digest = digest;
            h.basis = basis;
            h.stamps = stamps;
            h.off = off;
            return h;
        }

        void clear() {
            values = null;
            digest = "";
            basis = "";
            stamps = Map.of();
        }
    }

    private RollSecrets(Builder b) {
        this.api = b.api.replaceAll("/+$", "");
        this.http = b.http != null ? b.http : HttpClient.newBuilder().connectTimeout(Duration.ofSeconds(10)).build();
        this.timeout = b.timeout;
        this.cacheTtl = b.cacheTtl;
        this.clock = b.clock;
        this.token = b.token;
        this.project = b.project;
        this.login = b.token == null && b.identity != null ? new Relogin(b.identity, b.oidcToken) : null;
    }

    /**
     * A builder that starts from the environment, as {@code roll run} does: {@code ROLL_API_URL},
     * then {@code ROLL_TOKEN}, or {@code ROLL_IDENTITY} with the workload's OIDC token from
     * {@code ROLL_OIDC_TOKEN}, {@code ROLL_OIDC_TOKEN_FILE} (a Kubernetes projected token) or
     * GitHub Actions; {@code ROLL_PROJECT} names the project when the token doesn't. Anything set
     * on the builder wins.
     */
    public static Builder builder() {
        return new Builder(System::getenv);
    }

    static Builder builder(Function<String, String> env) {
        return new Builder(env);
    }

    /** How a client is made. */
    public static final class Builder {
        private final Function<String, String> env;
        private String api;
        private String token;
        private String project;
        private String identity;
        private Supplier<String> oidcToken;
        private HttpClient http;
        private Duration timeout = Duration.ofSeconds(30);
        private Duration cacheTtl = Duration.ZERO;
        private Clock clock = Clock.systemUTC();

        private Builder(Function<String, String> env) {
            this.env = env;
            this.api = read("ROLL_API_URL");
            this.token = read("ROLL_TOKEN");
            this.project = read("ROLL_PROJECT");
            this.identity = read("ROLL_IDENTITY");
        }

        private String read(String k) {
            String v = env.apply(k);
            return v == null || v.isBlank() ? null : v.trim();
        }

        /** rolld's API, e.g. {@code https://api.roll.example} (no trailing {@code /v1}). */
        public Builder api(String api) {
            this.api = api;
            return this;
        }

        /** A roll token with a rule on the project, or the operator token. */
        public Builder token(String token) {
            this.token = token;
            return this;
        }

        /** The project's id ({@code prj_…}). A token with a rule on one project knows it. */
        public Builder project(String project) {
            this.project = project;
            return this;
        }

        /**
         * Log in as a workload identity ({@code idn_…}) with the workload's own OIDC token instead
         * of a token: the client logs in again before the roll token ends, and when the server
         * refuses it. Without {@link #oidcToken}, the OIDC token comes from the environment.
         */
        public Builder identity(String identity) {
            this.identity = identity;
            return this;
        }

        /** Where the identity's OIDC token comes from; asked again for each login. */
        public Builder oidcToken(Supplier<String> oidcToken) {
            this.oidcToken = oidcToken;
            return this;
        }

        /** The HTTP client requests go through. */
        public Builder httpClient(HttpClient http) {
            this.http = http;
            return this;
        }

        /** Each request's timeout (default 30 s); a long poll gets its wait on top. */
        public Builder timeout(Duration timeout) {
            this.timeout = Objects.requireNonNull(timeout);
            return this;
        }

        /**
         * Keep {@link #values} answers this long and answer from memory meanwhile; after that, ask
         * again with the answer's digest and every key's stamp ({@code …/values/delta}): an
         * unchanged folder costs what a 304 does, and a changed one only its changed keys. A folder
         * holding dynamic credentials is never kept (they end with their lease). Zero (the
         * default): off.
         */
        public Builder cacheTtl(Duration cacheTtl) {
            this.cacheTtl = Objects.requireNonNull(cacheTtl);
            return this;
        }

        /** The clock the cache's TTL is measured with (for tests). */
        public Builder clock(Clock clock) {
            this.clock = Objects.requireNonNull(clock);
            return this;
        }

        public RollSecrets build() {
            if (api == null || api.isBlank()) {
                throw new RollSecretsException(400, "config", "set api, or ROLL_API_URL");
            }
            if (token == null && identity == null) {
                throw new RollSecretsException(400, "config", "set a token (ROLL_TOKEN), or an identity (ROLL_IDENTITY) for a workload identity");
            }
            if (token == null && oidcToken == null) {
                String a = api.replaceAll("/+$", "");
                oidcToken = () -> Oidc.token(a, this::read);
            }
            return new RollSecrets(this);
        }
    }

    // Requests.

    /** What one request answered: its status, its body and its ETag. */
    private record Answer(int status, String body, String etag) { }

    private Answer send(String token, String method, String path, Map<String, Object> body, Map<String, String> headers, Duration wait) {
        HttpRequest.Builder r = HttpRequest.newBuilder(URI.create(api + path)).timeout(timeout.plus(wait));
        if (token != null && !token.isEmpty()) {
            r.header("Authorization", "Bearer " + token);
        }
        headers.forEach(r::header);
        if (body != null) {
            r.header("Content-Type", "application/json");
            r.method(method, HttpRequest.BodyPublishers.ofString(Json.write(body)));
        } else {
            r.method(method, HttpRequest.BodyPublishers.noBody());
        }
        HttpResponse<InputStream> res;
        try {
            res = http.send(r.build(), HttpResponse.BodyHandlers.ofInputStream());
        } catch (IOException e) {
            throw new RollSecretsException(0, "io", "roll secrets didn't answer: " + e.getClass().getSimpleName(), e);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new RollSecretsException(0, "interrupted", "interrupted");
        }
        String text;
        try (InputStream in = res.body()) {
            byte[] data = in.readNBytes(MAX_BODY + 1);
            if (data.length > MAX_BODY) {
                throw new RollSecretsException(res.statusCode(), "too_large", "the answer is over 16 MiB");
            }
            text = new String(data, StandardCharsets.UTF_8);
        } catch (IOException e) {
            throw new RollSecretsException(0, "io", "the answer was cut off: " + e.getClass().getSimpleName(), e);
        }
        String etag = res.headers().firstValue("ETag").orElse(null);
        if (etag != null) {
            etag = etag.replaceFirst("^W/", "").replace("\"", "");
        }
        int status = res.statusCode();
        if (status >= 300 && status != 304) {
            String message = null;
            String code = null;
            try {
                Map<String, Object> e = Json.object(text);
                message = Json.str(e, "error");
                code = Json.str(e, "code");
            } catch (IllegalArgumentException notJson) {
                // the status says it
            }
            throw new RollSecretsException(status, code, message != null ? message : "HTTP " + status);
        }
        return new Answer(status, text, etag);
    }

    /**
     * Runs {@code request} with the client's token. A logged-in client's is renewed first when
     * it's ending, and when the server refuses it: one new login, then once more.
     */
    private Answer authed(Function<String, Answer> request) {
        if (login == null) {
            return request.apply(token);
        }
        String t = login.token(null);
        try {
            return request.apply(t);
        } catch (RollSecretsException e) {
            if (e.status() != 401) {
                throw e;
            }
            return request.apply(login.token(t));
        }
    }

    Map<String, Object> call(String method, String path, Map<String, Object> body) {
        return call(method, path, body, Duration.ZERO);
    }

    private Map<String, Object> call(String method, String path, Map<String, Object> body, Duration wait) {
        Answer a = authed(t -> send(t, method, path, body, Map.of(), wait));
        return a.body().isEmpty() ? Map.of() : parse(a.body());
    }

    /** A request whose answer is a JSON array of objects. */
    List<Map<String, Object>> callList(String method, String path) {
        Answer a = authed(t -> send(t, method, path, null, Map.of(), Duration.ZERO));
        Object v;
        try {
            v = Json.parse(a.body());
        } catch (IllegalArgumentException e) {
            throw new RollSecretsException(0, "invalid", e.getMessage());
        }
        if (!(v instanceof List)) {
            throw new RollSecretsException(0, "invalid", "the answer isn't a list");
        }
        return Json.objects(Map.of("items", v), "items");
    }

    private static Map<String, Object> parse(String body) {
        try {
            return Json.object(body);
        } catch (IllegalArgumentException e) {
            throw new RollSecretsException(0, "invalid", e.getMessage());
        }
    }

    static String seg(String s) {
        return URLEncoder.encode(s, StandardCharsets.UTF_8).replace("+", "%20");
    }

    private static String query(String path) {
        return path == null || path.isEmpty() ? "" : "?path=" + URLEncoder.encode(path, StandardCharsets.UTF_8);
    }

    private static String orRoot(String path) {
        return path == null || path.isEmpty() ? "/" : path;
    }

    /** The project: as given, or the token's own ({@code GET /v1/secrets/whoami}). */
    public String projectId() {
        String p = project;
        if (p != null) {
            return p;
        }
        if (login != null) {
            login.token(null); // a login names its identity's project
        }
        synchronized (this) {
            if (project == null) {
                String own = Json.str(call("GET", "/v1/secrets/whoami", null), "project");
                if (own == null || own.isEmpty()) {
                    throw new RollSecretsException(400, "project", "this token isn't limited to one project: set project");
                }
                project = own;
            }
            return project;
        }
    }

    private String env(String env) {
        return "/v1/secrets/projects/" + seg(projectId()) + "/environments/" + seg(env);
    }

    // Reading.

    /** {@code env}'s values at the root, references resolved. */
    public Values values(String env) {
        return values(env, "");
    }

    /** {@code env}'s values under {@code path} ({@code ""} for the root), references resolved. */
    public Values values(String env, String path) {
        return values(env, path, true);
    }

    /**
     * {@link #values(String, String)}; with {@code dynamic} false, a dynamic key is left out (and
     * named in {@link Values#dynamic()}) instead of issuing a fresh credential, for a reader that
     * won't renew leases ({@code ?dynamic=false}).
     */
    public Values values(String env, String path, boolean dynamic) {
        String e = env(env);
        String q = valuesQuery(path, dynamic);
        if (cacheTtl.isZero() || cacheTtl.isNegative()) {
            return Values.of(call("GET", e + "/values" + q, null), null);
        }
        return cached(e, q, dynamic);
    }

    private static String valuesQuery(String path, boolean dynamic) {
        String q = query(path);
        return q + (dynamic ? "" : (q.isEmpty() ? "?" : "&") + "dynamic=false");
    }

    private String valuesUrl(String env, String path, boolean dynamic) {
        return env(env) + "/values" + valuesQuery(path, dynamic);
    }

    /** A fresh read (a watch that saw the digest move): a kept answer for the folder goes. */
    private Values freshValues(String env, String path, boolean dynamic) {
        String url = valuesUrl(env, path, dynamic);
        Values v = Values.of(call("GET", url, null), null);
        cache.remove(url);
        return v;
    }

    /**
     * From memory within cacheTtl, else asks for what changed since the kept answer
     * ({@code …/values/delta}; the full set when nothing is kept). Where that fails, a conditional
     * read: a 304 keeps the values. {@code e} is the environment's route, {@code q} the query.
     */
    private Values cached(String e, String q, boolean dynamic) {
        String url = e + "/values" + q;
        Cached hit = cache.get(url);
        if (hit != null && clock.millis() - hit.at() < cacheTtl.toMillis()) {
            return hit.held().values;
        }
        boolean off = hit != null && hit.held().off;
        if (!off && !noDelta) {
            Held h = hit != null ? hit.held().copy() : new Held();
            Values v = tryDelta(e + "/values/delta" + q, h);
            if (v != null) {
                keep(url, h, dynamic);
                return v;
            }
            off = h.off;
        }
        String sep = url.contains("?") ? "&" : "?";
        Map<String, String> headers = hit != null && !hit.held().digest.isEmpty() ? Map.of("If-None-Match", "\"" + hit.held().digest + "\"") : Map.of();
        Answer a = authed(t -> send(t, "GET", url + sep + "digest=1", null, headers, Duration.ZERO));
        if (a.status() == 304 && hit != null) {
            Held k = hit.held().copy();
            k.off = off;
            keep(url, k, dynamic);
            return k.values;
        }
        Values v = Values.of(parse(a.body()), null);
        if (a.etag() == null) {
            cache.remove(url);
            return v;
        }
        Held k = new Held();
        k.values = v.digest().isEmpty() ? v.with(v.values(), a.etag()) : v;
        k.digest = a.etag();
        k.off = off;
        keep(url, k, dynamic);
        return k.values;
    }

    /**
     * Keeps an answer for cacheTtl, unless it holds dynamic credentials (they end with their
     * lease; with {@code dynamic=false} there are none, only their names) or a key that didn't
     * resolve (the digest may not move when the link recovers).
     */
    private void keep(String url, Held h, boolean dynamic) {
        if ((dynamic && !h.values.dynamic().isEmpty()) || !h.values.problems().isEmpty()) {
            cache.remove(url);
        } else {
            cache.put(url, new Cached(h, clock.millis()));
        }
    }

    /**
     * {@link #readDelta}, or null where it fails and the caller reads in full instead: a rolld
     * without the route (404, 405) from then on, a claim past what it takes (413) for that folder,
     * anything else this once. A stopped watch's interrupt is thrown.
     */
    private Values tryDelta(String url, Held h) {
        try {
            return readDelta(url, h);
        } catch (RollSecretsException e) {
            if ("interrupted".equals(e.code())) {
                throw e;
            }
            if (e.status() == 404 || e.status() == 405) {
                noDelta = true;
            } else if (e.status() == 413) {
                h.off = true;
            }
            return null;
        } catch (RuntimeException e) {
            return null;
        }
    }

    /**
     * Asks {@code url} ({@code …/values/delta?…}) for what changed since the answer {@code h}
     * holds and merges it in, leaving the new answer in {@code h}: the full set when {@code h}
     * holds nothing it may claim (nothing yet, problems, no basis) or when rolld answers it. On
     * any failure {@code h} holds nothing, so the next claim asks for the full set.
     */
    private Values readDelta(String url, Held h) {
        Map<String, Object> claim = new LinkedHashMap<>();
        if (h.values != null && !h.basis.isEmpty() && h.values.problems().isEmpty()) {
            claim.put("digest", h.digest);
            claim.put("basis", h.basis);
            claim.put("stamps", h.stamps);
        }
        Map<String, Object> a;
        try {
            a = call("POST", url, claim);
        } catch (RuntimeException e) {
            h.clear();
            throw e;
        }
        Values v = Values.of(a, null);
        Map<String, String> stamps = new LinkedHashMap<>();
        Map<String, Object> raw = Json.obj(a, "stamps");
        if (raw != null) {
            raw.forEach((k, s) -> {
                if (s instanceof String) {
                    stamps.put(k, (String) s);
                }
            });
        }
        Map<String, String> values = v.values();
        Map<String, String> kept = stamps;
        if (Boolean.TRUE.equals(a.get("delta")) && h.values != null) {
            values = new LinkedHashMap<>(h.values.values());
            kept = new LinkedHashMap<>(h.stamps);
            for (String k : Json.strings(a, "removed")) {
                values.remove(k);
                kept.remove(k);
            }
            values.putAll(v.values());
            kept.putAll(stamps);
        }
        String digest = Json.str(a, "digest");
        String basis = Json.str(a, "basis");
        v = v.with(values, digest);
        h.values = v;
        h.digest = v.digest();
        h.basis = basis == null ? "" : basis;
        h.stamps = Map.copyOf(kept);
        return v;
    }

    /**
     * One key's value under {@code path}, references resolved ({@code GET …/secrets/{key}/value}:
     * one read, audited as such). For many keys, or a key read per request with
     * {@link Builder#cacheTtl}, read {@link #values} once instead.
     *
     * @throws RollSecretsException 404 when there's no such key, 409 {@code unresolved} when it
     *     doesn't resolve
     */
    public String get(String env, String path, String key) {
        Map<String, Object> a = call("GET", env(env) + "/secrets/" + seg(key) + "/value" + query(path), null);
        List<Map<String, Object>> problems = Json.objects(a, "problems");
        if (!problems.isEmpty()) {
            Problem p = Problem.of(problems.get(0));
            throw new RollSecretsException(409, "unresolved", key + " doesn't resolve: " + p.problem());
        }
        return Json.str(a, "value");
    }

    /**
     * {@code env}'s values as {@code consumer} ({@code app:web}), what a running service gets:
     * dynamic credentials' leases are the consumer's, and the read is the consumer's in the audit
     * log.
     */
    public Values resolve(String env, String path, String consumer) {
        return resolve(env, path, consumer, true);
    }

    /** {@link #resolve(String, String, String)}; {@code dynamic} as in {@link #values(String, String, boolean)}. */
    public Values resolve(String env, String path, String consumer, boolean dynamic) {
        Map<String, Object> body = new LinkedHashMap<>();
        body.put("consumer", consumer);
        body.put("path", orRoot(path));
        if (!dynamic) {
            body.put("dynamic", false);
        }
        return Values.of(call("POST", env(env) + "/resolve", body), null);
    }

    /**
     * {@link #resolve} with the values sealed to {@code key} on the server and opened here: they
     * are never in the clear between the two.
     */
    public Values resolveSealed(String env, String path, String consumer, SealKey key) {
        Map<String, Object> body = new LinkedHashMap<>();
        body.put("consumer", consumer);
        body.put("path", orRoot(path));
        body.put("seal_to", key.publicKey());
        Map<String, Object> a = call("POST", env(env) + "/resolve", body);
        String context = Json.str(a, "context");
        byte[] plain = key.open(Sealed.of(Json.obj(a, "sealed")), context == null ? "" : context);
        Map<String, Object> values;
        try {
            values = Json.object(new String(plain, StandardCharsets.UTF_8));
        } catch (IllegalArgumentException e) {
            throw new RollSecretsException(0, "invalid", "the sealed values aren't a map");
        } finally {
            java.util.Arrays.fill(plain, (byte) 0);
        }
        return Values.of(a, values);
    }

    /** Moves whenever {@code env}'s values under {@code path} would; it reads no value. */
    public String digest(String env, String path) {
        return digestSince(env, path, null, Duration.ZERO);
    }

    /**
     * A long poll of the digest: it answers as soon as it differs from {@code since}, or with the
     * same one once {@code wait} (at most 25 s, the server's limit) runs out. A server without
     * long polls answers at once. No {@code since}, or a wait under a second: an ordinary read.
     */
    public String digestSince(String env, String path, String since, Duration wait) {
        String q = query(path);
        Duration held = Duration.ZERO;
        if (since != null && !since.isEmpty() && wait.compareTo(Duration.ofSeconds(1)) >= 0) {
            held = wait.compareTo(MAX_WAIT) > 0 ? MAX_WAIT : wait;
            q += (q.isEmpty() ? "?" : "&") + "since=" + URLEncoder.encode(since, StandardCharsets.UTF_8) + "&wait=" + held.toSeconds();
        }
        String d = Json.str(call("GET", env(env) + "/digest" + q, null, held), "digest");
        if (d == null) {
            throw new RollSecretsException(0, "invalid", "the answer has no digest");
        }
        return d;
    }

    /** Pushes a dynamic lease's expiry out by {@code ttlMinutes} (0: its link's TTL). */
    public Lease renew(String lease, int ttlMinutes) {
        Map<String, Object> body = new LinkedHashMap<>();
        if (ttlMinutes > 0) {
            body.put("ttl_minutes", (long) ttlMinutes);
        }
        return Lease.of(call("POST", "/v1/secrets/projects/" + seg(projectId()) + "/leases/" + seg(lease) + "/renew", body));
    }

    // Encryption keys.

    /**
     * The project's encryption key {@code name} (docs/secrets.md, "Encryption keys"): encrypt,
     * decrypt, sign, verify and HMAC through roll, the key never leaving it. Nothing is asked
     * until one of its operations is called.
     */
    public Transit transit(String name) {
        return new Transit(this, Objects.requireNonNull(name));
    }

    // Watching.

    /** {@link #watch(String, WatchOptions)} at the root with the defaults. */
    public Watch watch(String env, java.util.function.Consumer<Values> onChange) {
        return watch(env, WatchOptions.of(onChange));
    }

    /**
     * Calls {@code onChange} with {@code env}'s values now, then again whenever its digest moves,
     * renewing the dynamic leases of the latest values meanwhile (as {@code roll run --watch}
     * does), on a daemon thread until {@link Watch#close()}. It long-polls, so a change arrives
     * within a second or two; a server that answers at once is asked again every
     * {@link WatchOptions#every}.
     */
    public Watch watch(String env, WatchOptions o) {
        Watch w = new Watch(this, env, o);
        w.start();
        return w;
    }

    /**
     * A watch's read after the digest moved: as the consumer ({@code …/resolve} has no delta
     * route, so the whole folder), else only what changed since the answer {@code h} holds. Any
     * failure of the delta reads in full (a rolld without the route: from then on).
     */
    Values load(String env, WatchOptions o, Held h) {
        if (o.consumer != null) {
            return resolve(env, o.path, o.consumer, o.dynamic);
        }
        if (!h.off && !noDelta) {
            String e = env(env);
            String q = valuesQuery(o.path, o.dynamic);
            Values v = tryDelta(e + "/values/delta" + q, h);
            if (v != null) {
                // The newest answer: a kept one for the folder goes.
                cache.remove(e + "/values" + q);
                return v;
            }
        }
        return freshValues(env, o.path, o.dynamic);
    }

    // Workload identities.

    /**
     * Keeps a logged-in client's token fresh: a new login once less than a fifth of its life (at
     * least a minute) is left, or when the server refuses it.
     */
    private final class Relogin {
        private final String identity;
        private final Supplier<String> source;
        private long issued;
        private long expires;

        Relogin(String identity, Supplier<String> source) {
            this.identity = identity;
            this.source = source;
        }

        /** The client's token, renewed first when it's about to end (or {@code stale}: refused). */
        synchronized String token(String stale) {
            long now = System.currentTimeMillis();
            if (token == null) {
                renew();
                return token;
            }
            long margin = Math.max((expires - issued) / 5, 60_000);
            boolean ending = expires > 0 && now > expires - margin;
            boolean refused = stale != null && stale.equals(token);
            if (refused || ending) {
                try {
                    renew();
                } catch (RollSecretsException e) {
                    // Renewing early that fails keeps the token until it really ends (the identity
                    // provider or roll may be down for a moment).
                    if (refused || now >= expires) {
                        throw e;
                    }
                }
            }
            return token;
        }

        private void renew() {
            String oidc = source.get();
            Map<String, Object> body = new LinkedHashMap<>();
            body.put("identity", identity);
            body.put("token", oidc == null ? "" : oidc.trim());
            Map<String, Object> a = parse(send(null, "POST", "/v1/secrets/login", body, Map.of(), Duration.ZERO).body());
            String t = Json.str(a, "token");
            if (t == null || t.isEmpty()) {
                throw new RollSecretsException(0, "invalid", "the login answered no token");
            }
            token = t;
            issued = System.currentTimeMillis();
            expires = Json.num(a, "expires_at");
            if (project == null) {
                String p = Json.str(a, "project");
                if (p != null && !p.isEmpty()) {
                    project = p;
                }
            }
        }
    }

    /** Where a workload's OIDC token comes from when nothing else says. */
    static final class Oidc {
        private Oidc() { }

        static String token(String api, Function<String, String> env) {
            String t = env.apply("ROLL_OIDC_TOKEN");
            if (t != null) {
                return t;
            }
            String file = env.apply("ROLL_OIDC_TOKEN_FILE");
            if (file != null) {
                try {
                    // Read again for each login: Kubernetes replaces a projected token on disk.
                    return Files.readString(Path.of(file));
                } catch (IOException e) {
                    throw new RollSecretsException(0, "io", "ROLL_OIDC_TOKEN_FILE can't be read", e);
                }
            }
            String url = env.apply("ACTIONS_ID_TOKEN_REQUEST_URL");
            String bearer = env.apply("ACTIONS_ID_TOKEN_REQUEST_TOKEN");
            if (url == null || bearer == null) {
                throw new RollSecretsException(400, "config", "ROLL_IDENTITY is set but there's no OIDC token: set ROLL_OIDC_TOKEN or ROLL_OIDC_TOKEN_FILE, or run in GitHub Actions with `permissions: id-token: write`");
            }
            // The server's own URL by default: a token minted for one roll server isn't good at another.
            String audience = env.apply("ROLL_OIDC_AUDIENCE");
            if (audience == null) {
                audience = api;
            }
            HttpRequest r = HttpRequest.newBuilder(URI.create(url + (url.contains("?") ? "&" : "?") + "audience=" + URLEncoder.encode(audience, StandardCharsets.UTF_8)))
                    .timeout(Duration.ofSeconds(30)).header("Authorization", "Bearer " + bearer).GET().build();
            try {
                HttpResponse<InputStream> res = HttpClient.newHttpClient().send(r, HttpResponse.BodyHandlers.ofInputStream());
                try (InputStream in = res.body()) {
                    byte[] data = in.readNBytes(64 << 10);
                    if (res.statusCode() != 200) {
                        throw new RollSecretsException(0, "oidc", "GitHub Actions didn't give its OIDC token (does the job have `permissions: id-token: write`?)");
                    }
                    String v = Json.str(Json.object(new String(data, StandardCharsets.UTF_8)), "value");
                    if (v == null || v.isEmpty()) {
                        throw new IllegalArgumentException();
                    }
                    return v;
                }
            } catch (IllegalArgumentException e) {
                throw new RollSecretsException(0, "oidc", "GitHub Actions' OIDC answer has no value");
            } catch (IOException e) {
                throw new RollSecretsException(0, "io", "GitHub Actions didn't answer for its OIDC token", e);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                throw new RollSecretsException(0, "interrupted", "interrupted");
            }
        }
    }

    @Override
    public String toString() {
        return "RollSecrets[" + api + (project != null ? ", " + project : "") + "]";
    }
}
