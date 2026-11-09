// Testcontainers for Java's CassandraContainer (cqlsh readiness probe, init script, configuration
// override with PasswordAuthenticator) and ScyllaDBContainer (log wait, Alternator) with
// ROLL_NOSQL_IMAGE. Prints `PASS <check>` / `FAIL <check>: <why>`.

import com.datastax.oss.driver.api.core.CqlSession;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import org.testcontainers.cassandra.CassandraContainer;
import org.testcontainers.scylladb.ScyllaDBContainer;
import org.testcontainers.utility.DockerImageName;

public class TcCheck {
    static int failed = 0;

    interface Check { void run() throws Exception; }

    static void check(String name, Check c) {
        try {
            c.run();
            System.out.println("PASS " + name);
        } catch (Throwable e) {
            failed++;
            System.out.println(("FAIL " + name + ": " + e).replace('\n', ' '));
            e.printStackTrace();
        }
        System.out.flush();
    }

    static void expect(boolean c, String msg) {
        if (!c) throw new AssertionError(msg);
    }

    public static void main(String[] args) {
        DockerImageName image = DockerImageName.parse(System.getenv("ROLL_NOSQL_IMAGE"));
        check("testcontainers-java.cassandra", () -> {
            try (CassandraContainer c = new CassandraContainer(image.asCompatibleSubstituteFor("cassandra")).withInitScript("init.cql")) {
                c.start();
                try (CqlSession s = CqlSession.builder().addContactPoint(c.getContactPoint()).withLocalDatacenter(c.getLocalDatacenter()).build()) {
                    expect(s.execute("SELECT release_version FROM system.local").one().getString(0).equals("5.0.9"), "release_version");
                    expect("init script".equals(s.execute("SELECT name FROM tc.users WHERE id = 1").one().getString(0)), "init script applied");
                }
            }
        });
        check("testcontainers-java.cassandra-auth", () -> {
            try (CassandraContainer c = new CassandraContainer(image.asCompatibleSubstituteFor("cassandra")).withConfigurationOverride("cassandra-auth")) {
                c.start();
                try (CqlSession s = CqlSession.builder().addContactPoint(c.getContactPoint()).withLocalDatacenter(c.getLocalDatacenter())
                        .withAuthCredentials(c.getUsername(), c.getPassword()).build()) {
                    s.execute("CREATE ROLE app WITH PASSWORD = 'app' AND LOGIN = true");
                }
                boolean refused = false;
                try (CqlSession s = CqlSession.builder().addContactPoint(c.getContactPoint()).withLocalDatacenter(c.getLocalDatacenter()).build()) {
                    s.execute("SELECT * FROM system.local");
                } catch (Exception e) {
                    refused = true;
                }
                expect(refused, "connected without credentials");
            }
        });
        check("testcontainers-java.scylladb", () -> {
            try (ScyllaDBContainer c = new ScyllaDBContainer(image.asCompatibleSubstituteFor("scylladb/scylla")).withAlternator()) {
                c.start();
                try (CqlSession s = CqlSession.builder().addContactPoint(c.getContactPoint()).withLocalDatacenter("datacenter1").build()) {
                    expect(s.execute("SELECT release_version FROM system.local").one().getString(0).equals("3.0.8"), "release_version");
                    s.execute("CREATE KEYSPACE app WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}");
                    s.execute("CREATE TABLE app.t (k int PRIMARY KEY, v text)");
                    s.execute("INSERT INTO app.t (k, v) VALUES (1, 'one')");
                    expect("one".equals(s.execute("SELECT v FROM app.t WHERE k = 1").one().getString(0)), "row");
                }
                expect(c.getShardAwareContactPoint().getPort() > 0, "shard-aware port mapped");
                HttpResponse<String> r = HttpClient.newHttpClient().send(HttpRequest.newBuilder(URI.create(c.getAlternatorEndpoint() + "/localnodes")).build(), HttpResponse.BodyHandlers.ofString());
                expect(r.statusCode() == 200, "alternator /localnodes " + r.statusCode() + " " + r.body());
            }
        });
        System.exit(failed == 0 ? 0 : 1);
    }
}
