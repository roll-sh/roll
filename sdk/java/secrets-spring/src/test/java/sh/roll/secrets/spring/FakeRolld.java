package sh.roll.secrets.spring;

import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import java.io.IOException;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.URLDecoder;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.stream.Collectors;

/**
 * One project's folder in memory, as rolld answers it: values (a dynamic key only when the read
 * doesn't say {@code dynamic=false}), and a digest that long-polls (held 1.2 s, as rolld holds
 * it until it moves).
 */
final class FakeRolld implements AutoCloseable {
    static final String TOKEN = "rlt_fake_token_for_tests";
    static final String ENV = "/v1/secrets/projects/prj_test/environments/production";

    private final HttpServer server;
    private final ExecutorService pool = Executors.newCachedThreadPool(r -> {
        Thread t = new Thread(r, "fake-rolld");
        t.setDaemon(true);
        return t;
    });
    final List<String> seen = new ArrayList<>();
    private final Map<String, String> values = new LinkedHashMap<>();
    private final Map<String, String> dynamic = new LinkedHashMap<>();
    private int digest = 1;
    private volatile int status = 200;

    FakeRolld() throws IOException {
        server = HttpServer.create(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0), 0);
        server.setExecutor(pool);
        server.createContext("/", x -> {
            try {
                handle(x);
            } catch (Exception e) {
                x.sendResponseHeaders(500, -1);
            } finally {
                x.close();
            }
        });
        server.start();
    }

    String url() {
        return "http://127.0.0.1:" + server.getAddress().getPort();
    }

    synchronized void put(String key, String value) {
        values.put(key, value);
        digest++;
        notifyAll();
    }

    synchronized void putDynamic(String key, String value) {
        dynamic.put(key, value);
    }

    /** Every answer from now on is this status (403: the token has no rule on the folder). */
    void answerWith(int status) {
        this.status = status;
    }

    synchronized List<String> seen() {
        return new ArrayList<>(seen);
    }

    private void handle(HttpExchange x) throws Exception {
        String path = x.getRequestURI().getRawPath();
        Map<String, String> q = new LinkedHashMap<>();
        String raw = x.getRequestURI().getRawQuery();
        if (raw != null) {
            for (String part : raw.split("&")) {
                int eq = part.indexOf('=');
                q.put(URLDecoder.decode(eq < 0 ? part : part.substring(0, eq), StandardCharsets.UTF_8),
                        eq < 0 ? "" : URLDecoder.decode(part.substring(eq + 1), StandardCharsets.UTF_8));
            }
        }
        synchronized (this) {
            seen.add(x.getRequestMethod() + " " + x.getRequestURI());
        }
        if (!("Bearer " + TOKEN).equals(x.getRequestHeaders().getFirst("Authorization"))) {
            answer(x, 401, "{\"error\":\"unauthorized\",\"code\":\"unauthorized\"}");
            return;
        }
        if (status != 200) {
            answer(x, status, "{\"error\":\"this token has no rule on production/api\",\"code\":\"forbidden\"}");
            return;
        }
        if (!"/api".equals(q.get("path"))) {
            answer(x, 404, "{\"error\":\"no folder\",\"code\":\"not_found\"}");
            return;
        }
        switch (path) {
            case ENV + "/values": {
                Map<String, String> v;
                synchronized (this) {
                    v = new LinkedHashMap<>(values);
                    if (!"false".equals(q.get("dynamic"))) {
                        v.putAll(dynamic);
                    }
                    answer(x, 200, "{\"values\":" + object(v) + ",\"problems\":[],\"leases\":[],\"dynamic\":" + array(dynamic.keySet())
                            + ",\"linked\":[]}");
                }
                return;
            }
            case ENV + "/digest": {
                String d;
                synchronized (this) {
                    String since = q.get("since");
                    if (since != null && since.equals("d" + digest)) {
                        wait(1_200);
                    }
                    d = "d" + digest;
                }
                answer(x, 200, "{\"digest\":\"" + d + "\"}");
                return;
            }
            default:
                answer(x, 404, "{\"error\":\"not found\"}");
        }
    }

    private static void answer(HttpExchange x, int status, String body) throws IOException {
        byte[] b = body.getBytes(StandardCharsets.UTF_8);
        x.getResponseHeaders().set("Content-Type", "application/json");
        x.sendResponseHeaders(status, b.length);
        try (OutputStream out = x.getResponseBody()) {
            out.write(b);
        }
    }

    private static String object(Map<String, String> m) {
        return m.entrySet().stream().map(e -> "\"" + e.getKey() + "\":\"" + e.getValue() + "\"").collect(Collectors.joining(",", "{", "}"));
    }

    private static String array(java.util.Collection<String> keys) {
        return keys.stream().map(k -> "\"" + k + "\"").collect(Collectors.joining(",", "[", "]"));
    }

    @Override
    public void close() {
        server.stop(0);
        pool.shutdownNow();
    }
}
