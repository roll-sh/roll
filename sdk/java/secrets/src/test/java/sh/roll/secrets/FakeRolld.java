package sh.roll.secrets;

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

/** rolld as the client sees it, in memory: one handler answers every request. */
final class FakeRolld implements AutoCloseable {
    interface Handler {
        void handle(Request r) throws Exception;
    }

    /** One request, and how it's answered. */
    static final class Request {
        final HttpExchange x;
        final String method;
        final String path;
        final Map<String, String> query = new LinkedHashMap<>();
        final String body;

        Request(HttpExchange x) throws IOException {
            this.x = x;
            this.method = x.getRequestMethod();
            this.path = x.getRequestURI().getRawPath();
            String q = x.getRequestURI().getRawQuery();
            if (q != null) {
                for (String part : q.split("&")) {
                    int eq = part.indexOf('=');
                    String k = eq < 0 ? part : part.substring(0, eq);
                    String v = eq < 0 ? "" : part.substring(eq + 1);
                    query.put(URLDecoder.decode(k, StandardCharsets.UTF_8), URLDecoder.decode(v, StandardCharsets.UTF_8));
                }
            }
            this.body = new String(x.getRequestBody().readAllBytes(), StandardCharsets.UTF_8);
        }

        String header(String name) {
            return x.getRequestHeaders().getFirst(name);
        }

        Map<String, Object> json() {
            return Json.object(body);
        }

        void answer(int status, Object body) throws IOException {
            byte[] b = body == null ? new byte[0] : Json.write(body).getBytes(StandardCharsets.UTF_8);
            if (body != null) {
                x.getResponseHeaders().set("Content-Type", "application/json");
            }
            x.sendResponseHeaders(status, status == 304 || b.length == 0 ? -1 : b.length);
            try (OutputStream out = x.getResponseBody()) {
                out.write(b);
            }
        }

        void header(String name, String value) {
            x.getResponseHeaders().set(name, value);
        }
    }

    private final HttpServer server;
    private final ExecutorService pool = Executors.newCachedThreadPool(r -> {
        Thread t = new Thread(r, "fake-rolld");
        t.setDaemon(true);
        return t;
    });
    final List<String> seen = new ArrayList<>();

    FakeRolld(Handler handler) throws IOException {
        server = HttpServer.create(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0), 0);
        server.setExecutor(pool);
        server.createContext("/", x -> {
            try {
                Request r = new Request(x);
                synchronized (seen) {
                    seen.add(r.method + " " + x.getRequestURI());
                }
                handler.handle(r);
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

    @Override
    public void close() {
        server.stop(0);
        pool.shutdownNow();
    }

    static Map<String, Object> values(Map<String, Object> values, List<String> dynamic) {
        Map<String, Object> m = new LinkedHashMap<>();
        m.put("values", values);
        m.put("problems", List.of());
        m.put("leases", List.of());
        m.put("dynamic", dynamic);
        m.put("linked", List.of());
        return m;
    }
}
