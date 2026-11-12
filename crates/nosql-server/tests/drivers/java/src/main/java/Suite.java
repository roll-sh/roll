// The driver suite for Java: DataStax/Apache java-driver 4.x and ScyllaDB's fork (same API;
// ScyllaDB-only calls go through reflection). Same checks, names, environment and output as
// tests/drivers/python/suite.py: `PASS <name>`, `FAIL <name>: <why>`, `SKIP <name>: <why>`.

import com.datastax.oss.driver.api.core.AllNodesFailedException;
import com.datastax.oss.driver.api.core.CqlIdentifier;
import com.datastax.oss.driver.api.core.CqlSession;
import com.datastax.oss.driver.api.core.CqlSessionBuilder;
import com.datastax.oss.driver.api.core.auth.AuthenticationException;
import com.datastax.oss.driver.api.core.config.DefaultDriverOption;
import com.datastax.oss.driver.api.core.config.DriverConfigLoader;
import com.datastax.oss.driver.api.core.cql.*;
import com.datastax.oss.driver.api.core.data.CqlDuration;
import com.datastax.oss.driver.api.core.data.CqlVector;
import com.datastax.oss.driver.api.core.data.TupleValue;
import com.datastax.oss.driver.api.core.data.UdtValue;
import com.datastax.oss.driver.api.core.metadata.Node;
import com.datastax.oss.driver.api.core.metadata.TokenMap;
import com.datastax.oss.driver.api.core.metadata.schema.*;
import com.datastax.oss.driver.api.core.metadata.token.Token;
import com.datastax.oss.driver.api.core.type.DataTypes;
import com.datastax.oss.driver.api.core.type.TupleType;
import com.datastax.oss.driver.api.core.type.UserDefinedType;
import com.datastax.oss.driver.internal.core.metadata.token.Murmur3Token;
import com.datastax.oss.driver.internal.core.session.DefaultSession;

import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.math.BigDecimal;
import java.math.BigInteger;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.time.Duration;
import java.time.Instant;
import java.time.LocalDate;
import java.time.LocalTime;
import java.util.*;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.function.BooleanSupplier;

public class Suite {
    static final String HOST = env("HOST", "127.0.0.1");
    static final int PORT = Integer.parseInt(env("PORT", "9042"));
    static final String DIALECT = env("DIALECT", "scylla");
    static final String DC = env("DC", "datacenter1");
    static final String USER = env("USER_NAME", "");
    static final String PASS = env("PASS", "");
    static final boolean FEATURES = env("FEATURES", "").equals("1");
    static final int SHARDS = Integer.parseInt(env("SHARDS", "0").isEmpty() ? "0" : env("SHARDS", "0"));
    static final boolean RESTART = env("RESTART", "").equals("1");
    static final String KS = env("KS", "drv_java");
    static int failed = 0;
    static CqlSession s;

    static String env(String k, String d) {
        String v = System.getenv(k);
        return v == null ? d : v;
    }

    interface Check { Object run() throws Exception; }

    static void out(String line) {
        System.out.println(line);
        System.out.flush();
    }

    static void check(String name, Check c) {
        try {
            Object r = c.run();
            if (r instanceof String str && str.startsWith("SKIP")) out("SKIP " + name + ": " + str.substring(5));
            else out("PASS " + name);
        } catch (Throwable e) {
            failed++;
            String msg = (e.getClass().getSimpleName() + ": " + e.getMessage()).replace('\n', ' ');
            out("FAIL " + name + ": " + msg.substring(0, Math.min(400, msg.length())));
            if (!env("VERBOSE", "").isEmpty()) e.printStackTrace();
        }
    }

    static void expect(boolean cond, String msg) {
        if (!cond) throw new AssertionError(msg);
    }

    static void eventually(BooleanSupplier f, long timeoutMs) throws InterruptedException {
        long end = System.currentTimeMillis() + timeoutMs;
        while (true) {
            try {
                if (f.getAsBoolean()) return;
            } catch (RuntimeException e) {
                if (System.currentTimeMillis() > end) throw e;
            }
            if (System.currentTimeMillis() > end) throw new AssertionError("timed out");
            Thread.sleep(200);
        }
    }

    static CqlSessionBuilder builder(String user, String pass) {
        DriverConfigLoader cfg = DriverConfigLoader.programmaticBuilder()
            .withDuration(DefaultDriverOption.REQUEST_TIMEOUT, Duration.ofSeconds(30))
            .withDuration(DefaultDriverOption.CONNECTION_INIT_QUERY_TIMEOUT, Duration.ofSeconds(10))
            .withDuration(DefaultDriverOption.METADATA_SCHEMA_REQUEST_TIMEOUT, Duration.ofSeconds(30))
            .withDuration(DefaultDriverOption.CONTROL_CONNECTION_AGREEMENT_TIMEOUT, Duration.ofSeconds(10))
            .withDuration(DefaultDriverOption.METADATA_SCHEMA_WINDOW, Duration.ofMillis(200))
            .withDuration(DefaultDriverOption.RECONNECTION_BASE_DELAY, Duration.ofMillis(200))
            .withDuration(DefaultDriverOption.RECONNECTION_MAX_DELAY, Duration.ofSeconds(2))
            .build();
        CqlSessionBuilder b = CqlSession.builder().addContactPoint(new InetSocketAddress(HOST, PORT)).withLocalDatacenter(DC).withConfigLoader(cfg);
        if (!user.isEmpty()) b = b.withAuthCredentials(user, pass);
        return b;
    }

    static Object call(Object o, String method, Object... args) throws Exception {
        for (var m : o.getClass().getMethods()) {
            if (m.getName().equals(method) && m.getParameterCount() == args.length) {
                m.setAccessible(true);
                return m.invoke(o, args);
            }
        }
        throw new NoSuchMethodException(method);
    }

    static boolean has(Object o, String method) {
        return Arrays.stream(o.getClass().getMethods()).anyMatch(m -> m.getName().equals(method));
    }

    record T(String label, String type, Object value) {}

    public static void main(String[] argv) throws Exception {
        List<String> events = new CopyOnWriteArrayList<>();
        check("connect", () -> {
            s = builder(USER, PASS).withSchemaChangeListener(new com.datastax.oss.driver.api.core.metadata.schema.SchemaChangeListenerBase() {
                @Override public void onTableCreated(TableMetadata t) { events.add("created " + t.getName().asInternal()); }
                @Override public void onTableUpdated(TableMetadata t, TableMetadata prev) { events.add("updated " + t.getName().asInternal()); }
                @Override public void onTableDropped(TableMetadata t) { events.add("dropped " + t.getName().asInternal()); }
            }).build();
            return null;
        });
        if (s == null) System.exit(1);

        check("auth.bad_password", () -> {
            if (USER.isEmpty()) return "SKIP no authenticator";
            try (CqlSession bad = builder(USER, "wrong-password").build()) {
                throw new AssertionError("connected with a wrong password");
            } catch (AllNodesFailedException e) {
                Throwable cause = e.getAllErrors().values().iterator().next().get(0);
                expect(cause instanceof AuthenticationException, "error " + cause);
            }
            return null;
        });

        check("system.local", () -> {
            Row r = s.execute("SELECT release_version, data_center, rack FROM system.local").one();
            String want = DIALECT.equals("scylla") ? "3.0.8" : "5.0.9";
            expect(want.equals(r.getString("release_version")), "release_version " + r.getString("release_version"));
            expect(DC.equals(r.getString("data_center")), "data_center " + r.getString("data_center"));
            Node n = s.getMetadata().getNodes().values().iterator().next();
            expect(DC.equals(n.getDatacenter()) && r.getString("rack").equals(n.getRack()), "node " + n.getDatacenter() + "/" + n.getRack());
            return null;
        });

        s.execute("DROP KEYSPACE IF EXISTS " + KS);
        s.execute("CREATE KEYSPACE " + KS + " WITH replication = {'class': 'NetworkTopologyStrategy', '" + DC + "': 1}");
        s.execute("USE " + KS);
        s.execute("CREATE TYPE addr (street text, zip int, tags frozen<set<text>>)");
        s.execute("CREATE TABLE kv (k int, c int, v text, PRIMARY KEY (k, c)) WITH CLUSTERING ORDER BY (c DESC) AND comment = 'drivers'");
        s.execute("CREATE TABLE counters (k int PRIMARY KEY, n counter)");
        s.execute("CREATE TABLE lwt (k int PRIMARY KEY, v text, n int)");
        s.execute("CREATE INDEX kv_v ON kv (v)");

        List<T> types = List.of(
            new T("ascii", "ascii", "plain ascii"),
            new T("bigint", "bigint", Long.MIN_VALUE),
            new T("blob", "blob", ByteBuffer.wrap(new byte[] {0, 1, (byte) 0xfe, (byte) 0xff})),
            new T("boolean", "boolean", true),
            new T("date", "date", LocalDate.of(2024, 2, 29)),
            new T("decimal", "decimal", new BigDecimal("-1234567890.0987654321")),
            new T("double", "double", 1.5e300),
            new T("duration", "duration", CqlDuration.newInstance(1, 2, 3_000_000_004L)),
            new T("float", "float", 0.5f),
            new T("inet4", "inet", InetAddress.getByName("192.168.1.10")),
            new T("inet6", "inet", InetAddress.getByName("2001:db8::1")),
            new T("int", "int", Integer.MIN_VALUE),
            new T("smallint", "smallint", Short.MIN_VALUE),
            new T("text", "text", "texte unicode é中😀"),
            new T("time", "time", LocalTime.of(23, 59, 59, 999_999_999)),
            new T("timestamp", "timestamp", Instant.parse("2026-10-20T08:00:01.234Z")),
            new T("timeuuid", "timeuuid", UUID.fromString("d2177dd0-eaa2-11de-a572-001b779c76e3")),
            new T("tinyint", "tinyint", Byte.MIN_VALUE),
            new T("uuid", "uuid", UUID.fromString("01234567-89ab-cdef-0123-456789abcdef")),
            new T("varchar", "varchar", "varchar"),
            new T("varint", "varint", new BigInteger("123456789012345678901234567890")),
            new T("list", "list<int>", List.of(3, 1, 2)),
            new T("set", "set<text>", Set.of("b", "a")),
            new T("map", "map<text, int>", Map.of("x", 1, "y", 2)),
            new T("nested", "frozen<list<frozen<map<int, text>>>>", List.of(Map.of(1, "one"), Map.of(2, "two"))),
            new T("tuple", "tuple<int, text, double>", null),
            new T("udt", "frozen<addr>", null),
            new T("udt_nonfrozen", "addr", null),
            new T("vector", "vector<float, 3>", CqlVector.newInstance(0.5f, 1.0f, -2.0f)));
        StringBuilder cols = new StringBuilder();
        for (T t : types) cols.append(", c_").append(t.label).append(' ').append(t.type);
        s.execute("CREATE TABLE types (k int PRIMARY KEY" + cols + ")");

        check("metadata.table", () -> {
            s.refreshSchema();
            KeyspaceMetadata ks = s.getMetadata().getKeyspace(KS).orElseThrow();
            expect(ks.getReplication().get("class").endsWith("NetworkTopologyStrategy"), "replication " + ks.getReplication());
            TableMetadata t = ks.getTable("kv").orElseThrow();
            expect(t.getPartitionKey().get(0).getName().asInternal().equals("k"), "partition key");
            var ck = t.getClusteringColumns();
            expect(ck.size() == 1 && ck.values().iterator().next() == ClusteringOrder.DESC, "clustering " + ck);
            expect(t.getColumn("v").orElseThrow().getType().equals(DataTypes.TEXT), "v type");
            expect("drivers".equals(t.getOptions().get(CqlIdentifier.fromCql("comment"))), "comment " + t.getOptions().get(CqlIdentifier.fromCql("comment")));
            expect(t.describe(true).contains("CLUSTERING ORDER BY (c DESC)"), t.describe(true));
            TableMetadata tt = ks.getTable("types").orElseThrow();
            for (T x : types) {
                // varchar is stored as text; tuples are always frozen.
                String want = x.type.equals("varchar") ? "text" : x.type.startsWith("tuple") ? "frozen<" + x.type + ">" : x.type;
                String got = tt.getColumn("c_" + x.label).orElseThrow().getType().asCql(true, true).replace(KS + ".", "");
                expect(got.equals(want), x.label + ": " + got + " != " + want);
            }
            return null;
        });

        check("metadata.udt", () -> {
            UserDefinedType u = s.getMetadata().getKeyspace(KS).orElseThrow().getUserDefinedType("addr").orElseThrow();
            expect(u.getFieldNames().toString().equals("[street, zip, tags]"), "fields " + u.getFieldNames());
            expect(u.getFieldTypes().get(2).asCql(true, true).equals("frozen<set<text>>"), "tags " + u.getFieldTypes().get(2));
            return null;
        });

        check("metadata.index", () -> {
            TableMetadata t = s.getMetadata().getKeyspace(KS).orElseThrow().getTable("kv").orElseThrow();
            IndexMetadata i = t.getIndexes().get(CqlIdentifier.fromCql("kv_v"));
            expect(i != null && i.getKind() == IndexKind.COMPOSITES && i.getTarget().equals("v"), "index " + i);
            return null;
        });

        check("metadata.view", () -> {
            if (!FEATURES && DIALECT.equals("cassandra")) return "SKIP materialized views are disabled";
            s.execute("CREATE MATERIALIZED VIEW kv_by_v AS SELECT k, c, v FROM kv WHERE v IS NOT NULL AND k IS NOT NULL AND c IS NOT NULL PRIMARY KEY (v, k, c)");
            s.refreshSchema();
            ViewMetadata v = s.getMetadata().getKeyspace(KS).orElseThrow().getView("kv_by_v").orElseThrow();
            expect(v.getBaseTable().asInternal().equals("kv"), "base " + v.getBaseTable());
            expect(v.getPartitionKey().get(0).getName().asInternal().equals("v") && v.getClusteringColumns().size() == 2, "view keys");
            return null;
        });

        check("metadata.functions", () -> {
            if (!FEATURES) return "SKIP UDFs are disabled";
            if (DIALECT.equals("scylla")) {
                s.execute("CREATE FUNCTION twice(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x * 2'");
                s.execute("CREATE FUNCTION plus(acc int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE lua AS 'return (acc or 0) + (x or 0)'");
            } else {
                s.execute("CREATE FUNCTION twice(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return x * 2;'");
                s.execute("CREATE FUNCTION plus(acc int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE java AS 'return (acc == null ? 0 : acc) + (x == null ? 0 : x);'");
            }
            s.execute("CREATE AGGREGATE total(int) SFUNC plus STYPE int INITCOND 0");
            s.refreshSchema();
            KeyspaceMetadata ks = s.getMetadata().getKeyspace(KS).orElseThrow();
            FunctionMetadata f = ks.getFunction("twice", DataTypes.INT).orElseThrow();
            expect(f.getParameterNames().get(0).asInternal().equals("x") && f.getReturnType().equals(DataTypes.INT), "function");
            expect(f.getLanguage().equals(DIALECT.equals("scylla") ? "lua" : "java"), "language " + f.getLanguage());
            AggregateMetadata a = ks.getAggregate("total", DataTypes.INT).orElseThrow();
            expect(a.getStateFuncSignature().getName().asInternal().equals("plus") && a.getStateType().equals(DataTypes.INT), "aggregate");
            expect(Integer.valueOf(0).equals(a.getInitCond().orElseThrow()), "initcond " + a.getInitCond());
            return null;
        });

        check("schema_agreement", () -> {
            expect(s.checkSchemaAgreement(), "no agreement");
            ResultSet rs = s.execute("CREATE TABLE agree (k int PRIMARY KEY)");
            expect(rs.getExecutionInfo().isSchemaInAgreement(), "no agreement after DDL");
            return null;
        });

        check("prepared", () -> {
            PreparedStatement ins = s.prepare("INSERT INTO kv (k, c, v) VALUES (?, ?, ?)");
            List<String> names = new ArrayList<>();
            ins.getVariableDefinitions().forEach(d -> names.add(d.getName().asInternal()));
            expect(names.equals(List.of("k", "c", "v")), "bind metadata " + names);
            expect(ins.getPartitionKeyIndices().equals(List.of(0)), "routing key " + ins.getPartitionKeyIndices());
            for (int i = 0; i < 3; i++) s.execute(ins.bind(1, i, "v" + i));
            PreparedStatement sel = s.prepare("SELECT c, v FROM kv WHERE k = :k AND c >= :lo");
            List<String> got = new ArrayList<>();
            for (Row r : s.execute(sel.boundStatementBuilder().setInt("k", 1).setInt("lo", 1).build())) got.add(r.getInt("c") + "=" + r.getString("v"));
            expect(got.equals(List.of("2=v2", "1=v1")), "rows " + got);
            expect(sel.getResultSetDefinitions().size() == 2, "result metadata");
            return null;
        });

        check("paging", () -> {
            PreparedStatement ins = s.prepare("INSERT INTO kv (k, c, v) VALUES (2, ?, 'p')");
            for (int i = 0; i < 250; i++) s.execute(ins.bind(i));
            SimpleStatement st = SimpleStatement.newInstance("SELECT c FROM kv WHERE k = 2").setPageSize(50);
            ResultSet rs = s.execute(st);
            expect(rs.getAvailableWithoutFetching() == 50 && !rs.isFullyFetched(), "first page " + rs.getAvailableWithoutFetching());
            ByteBuffer state = rs.getExecutionInfo().getPagingState();
            ResultSet rs2 = s.execute(st.setPagingState(state));
            expect(rs2.one().getInt("c") == 199, "resumed page");
            List<Integer> all = new ArrayList<>();
            for (Row r : s.execute(st.setPagingState((ByteBuffer) null))) all.add(r.getInt(0));
            expect(all.size() == 250 && all.get(0) == 249 && all.get(249) == 0, all.size() + " rows");
            return null;
        });

        check("batches", () -> {
            PreparedStatement ins = s.prepare("INSERT INTO kv (k, c, v) VALUES (?, ?, ?)");
            s.execute(BatchStatement.newInstance(BatchType.LOGGED, ins.bind(3, 1, "b1"), SimpleStatement.newInstance("INSERT INTO kv (k, c, v) VALUES (4, 1, 'b2')")));
            s.execute(BatchStatement.newInstance(BatchType.UNLOGGED, ins.bind(3, 2, "u1"), ins.bind(3, 3, "u2")));
            PreparedStatement up = s.prepare("UPDATE counters SET n = n + ? WHERE k = ?");
            s.execute(BatchStatement.newInstance(BatchType.COUNTER, up.bind(5L, 1), up.bind(2L, 1)));
            expect(s.execute("SELECT * FROM kv WHERE k = 3").all().size() == 3, "logged+unlogged rows");
            expect(s.execute("SELECT n FROM counters WHERE k = 1").one().getLong(0) == 7, "counter");
            return null;
        });

        check("lwt", () -> {
            PreparedStatement ins = s.prepare("INSERT INTO lwt (k, v, n) VALUES (?, ?, ?) IF NOT EXISTS");
            expect(s.execute(ins.bind(1, "first", 1)).wasApplied(), "first insert applied");
            ResultSet r = s.execute(ins.bind(1, "second", 2));
            boolean applied = r.wasApplied();
            Row row = r.one();
            expect(!applied && "first".equals(row.getString("v")), "second insert");
            PreparedStatement up = s.prepare("UPDATE lwt SET v = ? WHERE k = ? IF n = ?");
            expect(s.execute(up.bind("third", 1, 1)).wasApplied(), "conditional update");
            expect(!s.execute(up.bind("fourth", 1, 5)).wasApplied(), "failed condition");
            if (has(ins, "isLWT") && DIALECT.equals("scylla")) {
                expect((Boolean) call(ins, "isLWT") && (Boolean) call(up, "isLWT"), "prepared LWT marking");
                expect(!(Boolean) call(s.prepare("SELECT v FROM lwt WHERE k = ?"), "isLWT"), "non-LWT marked");
            }
            return null;
        });

        KeyspaceMetadata ksm = s.getMetadata().getKeyspace(KS).orElseThrow();
        UserDefinedType addr = ksm.getUserDefinedType("addr").orElseThrow();
        for (int i = 0; i < types.size(); i++) {
            T t = types.get(i);
            int k = i;
            check("types." + t.label, () -> {
                Object value = t.value;
                if (t.label.equals("tuple")) {
                    TupleType tt = DataTypes.tupleOf(DataTypes.INT, DataTypes.TEXT, DataTypes.DOUBLE);
                    value = tt.newValue(1, "t", 2.5);
                } else if (t.label.startsWith("udt")) {
                    UserDefinedType ty = t.label.equals("udt") ? (UserDefinedType) addr.copy(true) : addr;
                    value = ty.newValue("Main St", 12345, Set.of("home", "work"));
                }
                PreparedStatement p = s.prepare("INSERT INTO types (k, c_" + t.label + ") VALUES (?, ?)");
                s.execute(p.bind(k, value));
                Object got = s.execute(s.prepare("SELECT c_" + t.label + " FROM types WHERE k = ?").bind(k)).one().getObject(0);
                if (value instanceof TupleValue tv) expect(tv.getFormattedContents().equals(((TupleValue) got).getFormattedContents()), got + " != " + value);
                else if (value instanceof UdtValue uv) {
                    UdtValue g = (UdtValue) got;
                    expect(Objects.equals(g.getString("street"), uv.getString("street")) && g.getInt("zip") == uv.getInt("zip")
                        && Objects.equals(g.getSet("tags", String.class), uv.getSet("tags", String.class)), g.getFormattedContents() + " != " + uv.getFormattedContents());
                }
                else expect(Objects.equals(got, value), got + " != " + value);
                return null;
            });
        }

        check("token_aware", () -> {
            TokenMap tm = s.getMetadata().getTokenMap().orElseThrow();
            PreparedStatement sel = s.prepare("SELECT token(k) FROM kv WHERE k = ?");
            PreparedStatement ins = s.prepare("INSERT INTO kv (k, c, v) VALUES (?, 0, 't')");
            for (int k : new int[] {1, 42, -7, 123456}) {
                s.execute(ins.bind(k));
                BoundStatement b = sel.bind(k);
                Token tok = tm.newToken(b.getRoutingKey());
                long server = s.execute(b).one().getLong(0);
                expect(((Murmur3Token) tok).getValue() == server, "k=" + k + ": driver " + tok + " server " + server);
                expect(tm.getReplicas(CqlIdentifier.fromCql(KS), tok).size() == 1, "replicas");
            }
            return null;
        });

        check("shard_aware", () -> {
            Node n = s.getMetadata().getNodes().values().iterator().next();
            if (!has(n, "getShardingInfo")) return "SKIP not a shard-aware driver";
            Object info = call(n, "getShardingInfo");
            if (DIALECT.equals("cassandra")) {
                expect(info == null, "Cassandra has no shards");
                return null;
            }
            expect(info != null && (Integer) call(info, "getShardsCount") == SHARDS, "shards " + (info == null ? null : call(info, "getShardsCount")));
            expect(Integer.valueOf(19042).equals(call(info, "getShardAwarePort")), "shard-aware port " + call(info, "getShardAwarePort"));
            var pool = ((DefaultSession) s).getPools().get(n);
            eventually(() -> pool.size() >= SHARDS, 15000);
            Set<Integer> shards = new TreeSet<>();
            TokenMap tm = s.getMetadata().getTokenMap().orElseThrow();
            PreparedStatement sel = s.prepare("SELECT v FROM kv WHERE k = ?");
            for (int k = 0; k < 40; k++) {
                Token tok = tm.newToken(sel.bind(k).getRoutingKey());
                int want = (Integer) call(info, "shardId", tok);
                Object ch = call(pool, "next", tok, want);
                expect(ch != null && (Integer) call(ch, "getShardId") == want, "key " + k + " routed to shard " + (ch == null ? null : call(ch, "getShardId")) + ", not " + want);
                shards.add(want);
            }
            expect(shards.size() == SHARDS, "keys spread over shards " + shards);
            return null;
        });

        check("tracing", () -> {
            ResultSet rs = s.execute(SimpleStatement.newInstance("SELECT * FROM kv WHERE k = 1").setTracing(true));
            QueryTrace tr = rs.getExecutionInfo().getQueryTrace();
            expect(tr.getRequestType() != null && tr.getCoordinatorAddress() != null && tr.getDurationMicros() >= 0, "trace");
            expect(!tr.getEvents().isEmpty(), "no trace events");
            return null;
        });

        check("events.schema", () -> {
            try (CqlSession other = builder(USER, PASS).build()) {
                other.execute("CREATE TABLE " + KS + ".evt (k int PRIMARY KEY, v text)");
                eventually(() -> events.contains("created evt") && s.getMetadata().getKeyspace(KS).get().getTable("evt").isPresent(), 15000);
                other.execute("ALTER TABLE " + KS + ".evt ADD w int");
                eventually(() -> events.contains("updated evt"), 15000);
                other.execute("DROP TABLE " + KS + ".evt");
                eventually(() -> events.contains("dropped evt"), 15000);
            }
            return null;
        });

        check("reconnect", () -> {
            if (!RESTART) return "SKIP the harness doesn't restart this server";
            PreparedStatement sel = s.prepare("SELECT v FROM kv WHERE k = ? AND c = ?");
            expect("v1".equals(s.execute(sel.bind(1, 1)).one().getString(0)), "before restart");
            out("@@RESTART");
            new BufferedReader(new InputStreamReader(System.in)).readLine();
            long end = System.currentTimeMillis() + 60000;
            while (true) {
                try {
                    Row r = s.execute(sel.bind(1, 1)).one();
                    expect(r != null && "v1".equals(r.getString(0)), "after restart");
                    break;
                } catch (AssertionError | RuntimeException e) {
                    if (System.currentTimeMillis() > end) throw e;
                    Thread.sleep(500);
                }
            }
            return null;
        });

        try {
            s.execute("DROP KEYSPACE IF EXISTS " + KS);
        } catch (RuntimeException ignored) {
        }
        s.close();
        out("DONE " + failed + " failed");
        System.exit(failed == 0 ? 0 : 1);
    }
}
