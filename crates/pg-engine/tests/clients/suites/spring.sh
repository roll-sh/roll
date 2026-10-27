# Spring JDBC 7.0.10 (through Spring Boot 4.1.2's auto-configured Hikari DataSource), Flyway
# 13.11.1 and Liquibase 5.0.4 (Java), with pgjdbc 42.7.14: a pinned driver program, since
# spring-jdbc's own tests run on H2 / HSQL and Flyway's / Liquibase's database tests need their
# Testcontainers setups. One PASS/FAIL line per check, in jdbc.lst, flyway.lst, liquibase.lst:
#   jdbc::*       JdbcTemplate, NamedParameterJdbcTemplate, SimpleJdbcInsert / SimpleJdbcCall
#                 (their DatabaseMetaData lookups), JdbcClient, key holders, batches, types,
#                 transactions (TransactionTemplate, savepoints), exception translation, timeouts;
#   flyway::*     migrate / info / validate / repair / clean / baseline over migrations with
#                 schemas, enums, domains, functions, triggers, views, a failing and a
#                 non-transactional migration, two concurrent migrates (its advisory lock);
#   liquibase::*  update / status / tag / rollback / validate / history / snapshot / diff /
#                 diffChangeLog / generateChangeLog / dropAll; snapshot::* and gen::* are one id
#                 per object or changeSet with a hash of its (normalised) text, so an object read
#                 differently from the catalogs shows as engine=MISSING. The generated changelog
#                 is also applied to a fresh database and diffed with the original (roundtrip).
# Databases: spring.setup.sql. Maven 3.9.16 is the src tarball; its repository is /cache/home/m2.
# image: eclipse-temurin:25-jdk
# src: https://archive.apache.org/dist/maven/maven-3/3.9.16/binaries/apache-maven-3.9.16-bin.tar.gz
set -eu
MVN="$SRC/bin/mvn -B -q -Dmaven.repo.local=/cache/home/m2"
W=/cache/work/spring
rm -rf $W && mkdir -p $W/src/main/java/t $W/flyway $W/lb && cd $W
cat > pom.xml <<'EOF'
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <parent>
    <groupId>org.springframework.boot</groupId>
    <artifactId>spring-boot-starter-parent</artifactId>
    <version>4.1.2</version>
  </parent>
  <groupId>t</groupId><artifactId>spring-pg</artifactId><version>1</version>
  <properties>
    <java.version>25</java.version>
    <flyway.version>13.11.1</flyway.version>
    <liquibase.version>5.0.4</liquibase.version>
    <postgresql.version>42.7.14</postgresql.version>
  </properties>
  <dependencies>
    <dependency><groupId>org.springframework.boot</groupId><artifactId>spring-boot-starter-jdbc</artifactId></dependency>
    <dependency><groupId>org.postgresql</groupId><artifactId>postgresql</artifactId></dependency>
    <dependency><groupId>org.flywaydb</groupId><artifactId>flyway-core</artifactId></dependency>
    <dependency><groupId>org.flywaydb</groupId><artifactId>flyway-database-postgresql</artifactId></dependency>
    <dependency><groupId>org.liquibase</groupId><artifactId>liquibase-core</artifactId></dependency>
  </dependencies>
</project>
EOF
cat > src/main/java/t/T.java <<'EOF'
package t;

import java.io.*;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.*;

/** PASS/FAIL lines into $OUT/<name>.lst; failure messages on stdout. */
final class T {
  interface Body { void run() throws Throwable; }
  static PrintWriter lst;
  static String prefix;
  static int pass, fail;

  static void open(String name) throws IOException {
    prefix = name + "::";
    lst = new PrintWriter(new FileWriter(System.getenv("OUT") + "/" + name + ".lst"), true);
  }

  static boolean check(String id, Body b) {
    try {
      b.run();
      lst.println("PASS " + prefix + id);
      pass++;
      return true;
    } catch (Throwable e) {
      lst.println("FAIL " + prefix + id);
      fail++;
      System.out.println("FAIL " + prefix + id + ": " + msg(e));
      return false;
    }
  }

  static String msg(Throwable e) {
    StringBuilder sb = new StringBuilder();
    for (Throwable c = e; c != null && sb.length() < 1500; c = c.getCause()) {
      sb.append(c.getClass().getSimpleName()).append(": ").append(c.getMessage()).append(" | ");
      if (c.getCause() == c) break;
    }
    return sb.toString().replace('\n', ' ');
  }

  /** One PASS line keyed by a hash of the text, the text itself in $OUT/<file>. */
  static void blob(String id, String text) {
    lst.println("PASS " + prefix + id + " " + sha(text));
  }

  static void save(String file, String text) throws IOException {
    try (Writer w = new OutputStreamWriter(new FileOutputStream(System.getenv("OUT") + "/" + file),
        StandardCharsets.UTF_8)) {
      w.write(text);
    }
  }

  static String sha(String s) {
    try {
      byte[] d = MessageDigest.getInstance("SHA-1").digest(s.getBytes(StandardCharsets.UTF_8));
      return HexFormat.of().formatHex(d).substring(0, 10);
    } catch (Exception e) {
      throw new RuntimeException(e);
    }
  }

  static void eq(Object exp, Object act) {
    if (!Objects.equals(String.valueOf(exp), String.valueOf(act)))
      throw new AssertionError("expected <" + exp + "> got <" + act + ">");
  }

  static void ok(boolean c, String what) {
    if (!c) throw new AssertionError(what);
  }

  static <E extends Throwable> E raises(Class<E> cls, Body b) {
    try {
      b.run();
    } catch (Throwable e) {
      if (cls.isInstance(e)) return cls.cast(e);
      throw new AssertionError("expected " + cls.getSimpleName() + " got " + msg(e), e);
    }
    throw new AssertionError("expected " + cls.getSimpleName() + ", nothing thrown");
  }

  static String url(String db) {
    return "jdbc:postgresql://" + System.getenv("PGHOST") + ":" + System.getenv("PGPORT") + "/" + db;
  }

  static void close() {
    System.out.println(prefix + " pass " + pass + " fail " + fail);
    lst.close();
    pass = fail = 0;
  }
}
EOF
cat > src/main/java/t/Main.java <<'EOF'
package t;

import org.springframework.boot.SpringApplication;
import org.springframework.boot.autoconfigure.SpringBootApplication;

@SpringBootApplication
public class Main {
  public static void main(String[] a) throws Exception {
    switch (a[0]) {
      case "jdbc" -> {
        var ctx = SpringApplication.run(Main.class,
            "--spring.datasource.url=" + T.url("spring_jdbc"),
            "--spring.datasource.username=postgres", "--spring.datasource.password=pw",
            "--spring.main.web-application-type=none", "--spring.main.banner-mode=off",
            "--logging.level.root=WARN");
        T.open("jdbc");
        JdbcChecks.run(ctx);
        JdbcChecks2.run(ctx);
        T.close();
        ctx.close();
      }
      case "flyway" -> { T.open("flyway"); FlywayChecks.run(a[1]); T.close(); }
      case "liquibase" -> { T.open("liquibase"); LiquibaseChecks.run(a[1]); T.close(); }
      default -> throw new IllegalArgumentException(a[0]);
    }
    System.exit(0);
  }
}
EOF
cat > src/main/java/t/Person.java <<'EOF'
package t;

import java.math.BigDecimal;
import java.time.LocalDate;

public class Person {
  private Long id;
  private String name;
  private Integer age;
  private LocalDate bornOn;
  private BigDecimal score;
  public Long getId() { return id; }
  public void setId(Long v) { id = v; }
  public String getName() { return name; }
  public void setName(String v) { name = v; }
  public Integer getAge() { return age; }
  public void setAge(Integer v) { age = v; }
  public LocalDate getBornOn() { return bornOn; }
  public void setBornOn(LocalDate v) { bornOn = v; }
  public BigDecimal getScore() { return score; }
  public void setScore(BigDecimal v) { score = v; }
  public static Person of(String n, int a) { Person p = new Person(); p.name = n; p.age = a; return p; }
}
EOF
cat > src/main/java/t/PersonRec.java <<'EOF'
package t;

public record PersonRec(Long id, String name, Integer age, String mood) {}
EOF
cat > src/main/java/t/JdbcChecks.java <<'EOF'
package t;

import static t.T.*;

import java.sql.*;
import java.time.LocalDate;
import java.util.*;
import javax.sql.DataSource;
import org.springframework.context.ConfigurableApplicationContext;
import org.springframework.jdbc.core.*;
import org.springframework.jdbc.core.namedparam.*;
import org.springframework.jdbc.core.simple.JdbcClient;
import org.springframework.jdbc.support.GeneratedKeyHolder;
import org.springframework.jdbc.support.JdbcUtils;

/** JdbcTemplate, NamedParameterJdbcTemplate, row mappers, key holders, batches. */
final class JdbcChecks {
  static final String[] SCHEMA = {
    "drop schema if exists other cascade", "drop table if exists pet, person cascade",
    "drop type if exists mood cascade",
    "create type mood as enum ('sad', 'ok', 'happy')",
    "create table person (id bigserial primary key, name text not null unique, age int,"
        + " mood mood, born_on date, seen timestamptz, score numeric(10,2), tags text[],"
        + " data jsonb, uid uuid default gen_random_uuid(), photo bytea, active boolean default true)",
    "create table pet (id serial primary key, owner_id bigint references person(id) on delete cascade,"
        + " kind varchar(20) not null check (kind <> ''), name text)",
    "create schema other",
    "create table other.item (id int generated always as identity primary key, label text,"
        + " created timestamp default now())",
    "create function add_nums(a int, b int, out total int, out product int) language plpgsql"
        + " as $$ begin total := a + b; product := a * b; end $$",
    "create function person_count() returns bigint language sql as 'select count(*) from person'",
    "create function people_older(min_age int) returns setof person language sql"
        + " as 'select * from person where age > min_age order by id'",
    "create function people_older_cur(min_age int) returns refcursor language plpgsql as $$ declare c refcursor;"
        + " begin open c for select name from person where age > min_age order by id; return c; end $$",
    "create procedure bump_age(p_id bigint, inout new_age int) language plpgsql as"
        + " $$ begin update person set age = age + 1 where id = p_id returning age into new_age; end $$",
  };

  static void run(ConfigurableApplicationContext ctx) {
    DataSource ds = ctx.getBean(DataSource.class);
    JdbcTemplate jdbc = ctx.getBean(JdbcTemplate.class);
    NamedParameterJdbcTemplate named = ctx.getBean(NamedParameterJdbcTemplate.class);
    check("boot::beans", () -> {
      eq("HikariDataSource", ds.getClass().getSimpleName());
      ok(ctx.getBean(JdbcClient.class) != null, "JdbcClient bean");
    });
    check("boot::metadata", () -> {
      eq("PostgreSQL", JdbcUtils.extractDatabaseMetaData(ds, DatabaseMetaData::getDatabaseProductName));
      eq(18, JdbcUtils.extractDatabaseMetaData(ds, DatabaseMetaData::getDatabaseMajorVersion));
    });
    check("ddl::schema", () -> { for (String s : SCHEMA) jdbc.execute(s); });
    check("template::update", () -> {
      eq(1, jdbc.update("insert into person (name, age, mood, born_on, score) values (?, ?, ?::mood, ?, ?)",
          "ann", 31, "happy", LocalDate.of(1994, 3, 2), new java.math.BigDecimal("12.50")));
      eq(1, jdbc.update("insert into person (name, age) values (?, ?)", "bob", 45));
      eq(1, jdbc.update("insert into person (name, age, mood) values (?, ?, 'sad')", "cid", 19));
    });
    check("template::queryForObject", () -> {
      eq(3, jdbc.queryForObject("select count(*) from person", Integer.class));
      eq("bob", jdbc.queryForObject("select name from person where age = ?", String.class, 45));
      eq(null, jdbc.queryForObject("select mood::text from person where name = ?", String.class, "bob"));
    });
    check("template::queryForList", () -> {
      eq("[ann, bob, cid]", jdbc.queryForList("select name from person order by name", String.class));
      List<Map<String, Object>> rows = jdbc.queryForList("select name, age from person where age > ? order by age", 20);
      eq("[{name=ann, age=31}, {name=bob, age=45}]", rows);
    });
    check("template::queryForMap", () -> {
      Map<String, Object> m = jdbc.queryForMap("select id, name, mood, score, born_on from person where name = 'ann'");
      eq("happy", m.get("mood").toString());
      eq("12.50", m.get("score"));
      eq("1994-03-02", m.get("born_on"));
      eq("Long", m.get("id").getClass().getSimpleName());
    });
    check("template::rowMapper", () -> eq("[ann:31, bob:45]",
        jdbc.query("select name, age from person where age > ? order by 1", (rs, i) ->
            rs.getString(1) + ":" + rs.getInt(2), 20)));
    check("template::extractor_callback", () -> {
      eq(95, jdbc.query("select age from person", rs -> { int s = 0; while (rs.next()) s += rs.getInt(1); return s; }));
      List<String> seen = new ArrayList<>();
      jdbc.query("select name from person order by id desc", rs -> { seen.add(rs.getString(1)); });
      eq("[cid, bob, ann]", seen);
    });
    check("template::rowSet", () -> {
      var rs = jdbc.queryForRowSet("select id, name, score, tags from person order by id");
      var md = rs.getMetaData();
      eq("id,name,score,tags", String.join(",", md.getColumnNames()));
      eq("bigserial,text,numeric,_text", md.getColumnTypeName(1) + "," + md.getColumnTypeName(2) + ","
          + md.getColumnTypeName(3) + "," + md.getColumnTypeName(4));
      int n = 0; while (rs.next()) n++;
      eq(3, n);
    });
    check("mapper::beanProperty", () -> {
      Person p = jdbc.queryForObject("select * from person where name = ?", new BeanPropertyRowMapper<>(Person.class), "ann");
      eq("ann 31 1994-03-02 12.50", p.getName() + " " + p.getAge() + " " + p.getBornOn() + " " + p.getScore());
    });
    check("mapper::dataClass", () -> eq("PersonRec[id=3, name=cid, age=19, mood=sad]",
        jdbc.queryForObject("select id, name, age, mood::text as mood from person where name = 'cid'",
            new DataClassRowMapper<>(PersonRec.class))));
    check("keys::column_names", () -> {
      var kh = new GeneratedKeyHolder();
      jdbc.update(c -> {
        var ps = c.prepareStatement("insert into person (name, age) values (?, ?)", new String[] {"id"});
        ps.setString(1, "dan"); ps.setInt(2, 52); return ps;
      }, kh);
      eq(4, kh.getKey());
    });
    check("keys::return_generated_keys", () -> {
      var kh = new GeneratedKeyHolder();
      jdbc.update(c -> {
        var ps = c.prepareStatement("insert into person (name, age) values (?, ?)", Statement.RETURN_GENERATED_KEYS);
        ps.setString(1, "eve"); ps.setInt(2, 28); return ps;
      }, kh);
      Map<String, Object> k = kh.getKeys();
      eq(5L, k.get("id")); eq(true, k.get("active")); ok(k.get("uid") instanceof UUID, "uid " + k.get("uid"));
    });
    check("keys::batch", () -> {
      var kh = new GeneratedKeyHolder();
      List<String> names = List.of("k1", "k2", "k3");
      jdbc.batchUpdate(c -> c.prepareStatement("insert into pet (owner_id, kind, name) values (1, 'cat', ?)", new String[] {"id"}),
          new BatchPreparedStatementSetter() {
            public void setValues(PreparedStatement ps, int i) throws SQLException { ps.setString(1, names.get(i)); }
            public int getBatchSize() { return names.size(); }
          }, kh);
      eq("[{id=1}, {id=2}, {id=3}]", kh.getKeyList());
    });
    check("batch::args", () -> {
      List<Object[]> args = new ArrayList<>();
      for (int i = 0; i < 50; i++) args.add(new Object[] {2, "dog", "d" + i});
      int[] r = jdbc.batchUpdate("insert into pet (owner_id, kind, name) values (?, ?, ?)", args);
      eq(50, Arrays.stream(r).sum());
    });
    check("batch::sized", () -> {
      List<Integer> xs = new ArrayList<>(); for (int i = 0; i < 25; i++) xs.add(i);
      int[][] r = jdbc.batchUpdate("update pet set name = name || '!' where id = ?", xs, 10, (ps, x) -> ps.setInt(1, x + 4));
      eq(3, r.length); eq(10, r[0].length); eq(5, r[2].length);
    });
    check("batch::statements", () -> {
      int[] r = jdbc.batchUpdate("update person set age = age where id = 1", "delete from pet where name = 'nobody'",
          "insert into pet (owner_id, kind) values (3, 'fish')");
      eq("[1, 0, 1]", Arrays.toString(r));
    });
    check("named::params", () -> {
      var p = new MapSqlParameterSource("min", 20).addValue("names", List.of("ann", "bob", "zed"));
      eq("[ann, bob]", named.queryForList("select name from person where age > :min and name in (:names) order by 1", p, String.class));
      eq(2, named.queryForObject("select count(*) from person where name in (:n) and age between :lo and :hi",
          Map.of("n", List.of("ann", "bob", "cid"), "lo", 20, "hi", 60), Integer.class));
    });
    check("named::beanSource_batch", () -> {
      List<Person> ps = List.of(Person.of("f1", 61), Person.of("f2", 62), Person.of("f3", 63));
      int[] r = named.batchUpdate("insert into person (name, age) values (:name, :age)", SqlParameterSourceUtils.createBatch(ps));
      eq(3, Arrays.stream(r).sum());
      var kh = new GeneratedKeyHolder();
      named.update("insert into person (name, age) values (:name, :age)", new BeanPropertySqlParameterSource(Person.of("f4", 64)), kh, new String[] {"id"});
      ok(kh.getKey().longValue() > 5, "key " + kh.getKey());
    });
    check("named::upsert_returning", () -> eq(32, named.queryForObject(
        "insert into person (name, age) values (:n, :a) on conflict (name) do update set age = excluded.age + 1 returning age",
        Map.of("n", "ann", "a", 31), Integer.class)));
  }
}
EOF
cat > src/main/java/t/JdbcChecks2.java <<'EOF'
package t;

import static t.T.*;

import java.math.BigDecimal;
import java.sql.*;
import java.time.*;
import java.util.*;
import javax.sql.DataSource;
import org.postgresql.util.PGobject;
import org.springframework.context.ConfigurableApplicationContext;
import org.springframework.core.io.ByteArrayResource;
import org.springframework.jdbc.core.*;
import org.springframework.jdbc.core.simple.*;
import org.springframework.jdbc.datasource.init.ResourceDatabasePopulator;
import org.springframework.jdbc.object.*;
import org.springframework.jdbc.support.GeneratedKeyHolder;

/** SimpleJdbcInsert / SimpleJdbcCall, JdbcClient, types, object queries, scripts. */
final class JdbcChecks2 {
  static void run(ConfigurableApplicationContext ctx) {
    DataSource ds = ctx.getBean(DataSource.class);
    JdbcTemplate jdbc = ctx.getBean(JdbcTemplate.class);
    JdbcClient client = ctx.getBean(JdbcClient.class);
    check("insert::returnKey", () -> {
      var ins = new SimpleJdbcInsert(ds).withTableName("person").usingColumns("name", "age", "score").usingGeneratedKeyColumns("id");
      Number k = ins.executeAndReturnKey(Map.of("name", "gus", "age", 70, "score", new BigDecimal("1.5")));
      eq("gus", jdbc.queryForObject("select name from person where id = ?", String.class, k.longValue()));
      eq("INSERT INTO person (name, age, score) VALUES(?, ?, ?)", ins.getInsertString());
    });
    check("insert::usingColumns_batch", () -> {
      var ins = new SimpleJdbcInsert(jdbc).withTableName("pet").usingColumns("owner_id", "kind", "name");
      int[] r = ins.executeBatch(Map.of("owner_id", 1, "kind", "owl", "name", "o1"),
          Map.of("owner_id", 2, "kind", "owl", "name", "o2"));
      eq(2, Arrays.stream(r).sum());
    });
    check("insert::schema_identity", () -> {
      var ins = new SimpleJdbcInsert(ds).withSchemaName("other").withTableName("item").usingGeneratedKeyColumns("id");
      eq(1, ins.executeAndReturnKey(Map.of("label", "first")));
      var kh = ins.executeAndReturnKeyHolder(Map.of("label", "second"));
      eq(2, kh.getKeys().get("id"));
    });
    check("call::out_params", () -> {
      Map<String, Object> r = new SimpleJdbcCall(ds).withFunctionName("add_nums").execute(3, 4);
      eq(7, r.get("total")); eq(12, r.get("product"));
    });
    check("call::function_return", () -> eq(jdbc.queryForObject("select count(*) from person", Long.class),
        new SimpleJdbcCall(jdbc).withFunctionName("person_count").executeFunction(Long.class)));
    check("call::refcursor", () -> {
      var tt = new org.springframework.transaction.support.TransactionTemplate(
          ctx.getBean(org.springframework.transaction.PlatformTransactionManager.class));
      Map<String, Object> r = tt.execute(s -> new SimpleJdbcCall(ds).withFunctionName("people_older_cur")
          .returningResultSet("rows", (rs, i) -> rs.getString(1)).execute(Map.of("min_age", 60)));
      eq("[f1, f2, f3, f4, gus]", r.get("rows"));
    });
    check("call::procedure_inout", () -> eq(46, jdbc.execute((Connection c) -> {
      try (var cs = c.prepareCall("call bump_age(?, ?)")) {
        cs.setLong(1, 2); cs.setNull(2, Types.INTEGER); cs.registerOutParameter(2, Types.INTEGER);
        cs.execute(); return cs.getInt(2);
      }
    })));
    check("client::mapped", () -> {
      List<PersonRec> ps = client.sql("select id, name, age, mood::text as mood from person where age < ? order by id")
          .param(30).query(PersonRec.class).list();
      eq("[cid, eve]", ps.stream().map(PersonRec::name).toList());
      eq("ann", client.sql("select * from person where id = :id").param("id", 1L).query(Person.class).single().getName());
      eq(false, client.sql("select name from person where id = -1").query(String.class).optional().isPresent());
    });
    check("client::rows_update_keys", () -> {
      eq(2, client.sql("select name, age from person where name in (:n)").param("n", List.of("ann", "bob")).query().listOfRows().size());
      var kh = new GeneratedKeyHolder();
      client.sql("insert into pet (owner_id, kind, name) values (:o, :k, :n)").param("o", 1).param("k", "rat").param("n", "r")
          .update(kh, "id", "kind");
      eq("rat", kh.getKeys().get("kind"));
      eq(1, client.sql("update person set active = ? where name = ?").params(false, "bob").update());
    });
    check("client::stream_fetch", () -> {
      var tt = new org.springframework.transaction.support.TransactionTemplate(
          ctx.getBean(org.springframework.transaction.PlatformTransactionManager.class));
      eq(500500L, tt.execute(s -> {
        try (var st = client.sql("select g from generate_series(1, 1000) g").withFetchSize(7).query(Long.class).stream()) {
          return st.mapToLong(Long::longValue).sum();
        }
      }));
      eq(5, client.sql("select g from generate_series(1, 1000) g").withMaxRows(5).query(Integer.class).list().size());
    });
    check("types::arrays", () -> {
      jdbc.update(c -> {
        var ps = c.prepareStatement("update person set tags = ? where name = 'ann'");
        ps.setArray(1, c.createArrayOf("text", new String[] {"a", "b c", null, "{x}"})); return ps;
      });
      eq("[a, b c, null, {x}]", jdbc.queryForObject("select tags from person where name = 'ann'",
          (rs, i) -> Arrays.toString((Object[]) rs.getArray(1).getArray())));
      eq("{1,2,3}", jdbc.queryForObject("select ?::int[]::text", String.class, (Object) new Integer[] {1, 2, 3}));
      eq(2, jdbc.queryForObject("select count(*) from person where name = any(?)", Integer.class,
          (Object) new String[] {"ann", "bob", "nope"}));
    });
    check("types::jsonb", () -> {
      var o = new PGobject(); o.setType("jsonb"); o.setValue("{\"k\": [1, 2, {\"z\": null}], \"a\": \"é\"}");
      jdbc.update("update person set data = ? where name = 'ann'", o);
      eq("{\"a\": \"é\", \"k\": [1, 2, {\"z\": null}]}", jdbc.queryForObject("select data from person where name = 'ann'", String.class));
      eq(2, jdbc.queryForObject("select (data->'k'->>1)::int from person where data @> ?::jsonb", Integer.class, "{\"a\":\"é\"}"));
    });
    check("types::uuid_bytea", () -> {
      UUID u = UUID.fromString("0e4f6a76-8f4a-4c2e-9d0e-3c5b1f6a7d10");
      byte[] b = {0, 1, 2, (byte) 255, 39, 92};
      jdbc.update("update person set uid = ?, photo = ? where name = 'bob'", u, b);
      eq(u, jdbc.queryForObject("select uid from person where name = 'bob'", UUID.class));
      eq(Arrays.toString(b), Arrays.toString(jdbc.queryForObject("select photo from person where name = 'bob'", byte[].class)));
    });
    check("types::time", () -> {
      var odt = OffsetDateTime.of(2024, 2, 29, 23, 59, 58, 123456000, ZoneOffset.ofHours(5));
      jdbc.update("update person set seen = ? where name = 'cid'", odt);
      eq(odt.toInstant(), jdbc.queryForObject("select seen from person where name = 'cid'", OffsetDateTime.class).toInstant());
      var ldt = LocalDateTime.of(1999, 12, 31, 12, 0, 1);
      eq(ldt, jdbc.queryForObject("select ?::timestamp", LocalDateTime.class, ldt));
      eq(LocalTime.of(13, 14, 15), jdbc.queryForObject("select time '13:14:15'", LocalTime.class));
      eq("1 year 2 mons 3 days 04:05:06", jdbc.queryForObject("select interval '1 year 2 months 3 days 4:05:06'", String.class));
    });
    check("types::numeric_enum", () -> {
      eq("123456789012345678901234567890.123", jdbc.queryForObject("select ?::numeric", BigDecimal.class,
          new BigDecimal("123456789012345678901234567890.123")));
      eq("NaN Infinity", jdbc.queryForObject("select 'NaN'::float8 || ' ' || 'Infinity'::float8", String.class));
      jdbc.update("update person set mood = ? where name = 'eve'", new SqlParameterValue(Types.OTHER, "ok"));
      eq("ok", jdbc.queryForObject("select mood from person where name = 'eve'", String.class));
    });
    check("types::prepared_reuse", () -> {
      for (int i = 0; i < 12; i++) eq(i * 2L, jdbc.queryForObject("select ?::bigint * 2", Long.class, (long) i));
      for (int i = 0; i < 8; i++) eq(i % 2 == 0, jdbc.queryForObject("select ? = 0", Boolean.class, i % 2));
    });
    check("object::mappingQuery_sqlUpdate", () -> {
      var q = new MappingSqlQuery<String>(ds, "select name from person where age > ? order by name") {
        protected String mapRow(ResultSet rs, int i) throws SQLException { return rs.getString(1); }
      };
      q.declareParameter(new SqlParameter(Types.INTEGER)); q.compile();
      eq("[f4, gus]", q.execute(63));
      var u = new SqlUpdate(ds, "update pet set name = upper(name) where kind = ?", new int[] {Types.VARCHAR});
      u.compile();
      eq(2, u.update("owl"));
    });
    check("script::populator", () -> {
      var p = new ResourceDatabasePopulator(new ByteArrayResource((
          "-- comment\ncreate table scripted (id int primary key, t text);\n"
          + "insert into scripted values (1, 'semi;colon'), (2, 'quote''s');\n/* block */ update scripted set t = t || '.';\n").getBytes()));
      p.execute(ds);
      eq("[semi;colon., quote's.]", jdbc.queryForList("select t from scripted order by id", String.class));
    });
  }
}
EOF
cat > src/main/java/t/JdbcChecks3.java <<'EOF'
package t;

import static t.T.*;

import java.util.concurrent.*;
import javax.sql.DataSource;
import org.springframework.context.ConfigurableApplicationContext;
import org.springframework.dao.*;
import org.springframework.jdbc.BadSqlGrammarException;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.transaction.*;
import org.springframework.transaction.support.*;

/** Transactions and exception translation. */
final class JdbcChecks3 {
  static void run(ConfigurableApplicationContext ctx) {
    DataSource ds = ctx.getBean(DataSource.class);
    JdbcTemplate jdbc = ctx.getBean(JdbcTemplate.class);
    PlatformTransactionManager tm = ctx.getBean(PlatformTransactionManager.class);
    jdbc.execute("create table acct (id int primary key, bal int not null check (bal >= 0))");
    jdbc.execute("insert into acct values (1, 100), (2, 50)");
    TransactionTemplate tt = new TransactionTemplate(tm);
    check("tx::commit", () -> {
      tt.executeWithoutResult(s -> { jdbc.update("update acct set bal = bal - 10 where id = 1"); jdbc.update("update acct set bal = bal + 10 where id = 2"); });
      eq("[90, 60]", jdbc.queryForList("select bal from acct order by id", Integer.class));
    });
    check("tx::rollbackOnly", () -> {
      tt.executeWithoutResult(s -> { jdbc.update("update acct set bal = 0"); s.setRollbackOnly(); });
      eq("[90, 60]", jdbc.queryForList("select bal from acct order by id", Integer.class));
    });
    check("tx::exception_rollback", () -> {
      raises(DataIntegrityViolationException.class, () -> tt.executeWithoutResult(s -> {
        jdbc.update("update acct set bal = bal + 1 where id = 2");
        jdbc.update("update acct set bal = bal - 1000 where id = 1");
      }));
      eq("[90, 60]", jdbc.queryForList("select bal from acct order by id", Integer.class));
    });
    check("tx::nested_savepoint", () -> {
      var nested = new TransactionTemplate(tm); nested.setPropagationBehavior(TransactionDefinition.PROPAGATION_NESTED);
      tt.executeWithoutResult(s -> {
        jdbc.update("update acct set bal = 91 where id = 1");
        try {
          nested.executeWithoutResult(n -> { jdbc.update("update acct set bal = 61 where id = 2"); jdbc.update("insert into acct values (1, 1)"); });
        } catch (DuplicateKeyException expected) { }
        jdbc.update("update acct set bal = bal + 1 where id = 1");
      });
      eq("[92, 60]", jdbc.queryForList("select bal from acct order by id", Integer.class));
    });
    check("tx::requires_new", () -> {
      var inner = new TransactionTemplate(tm); inner.setPropagationBehavior(TransactionDefinition.PROPAGATION_REQUIRES_NEW);
      tt.executeWithoutResult(s -> {
        jdbc.update("insert into acct values (3, 3)");
        inner.executeWithoutResult(n -> jdbc.update("insert into acct values (4, 4)"));
        s.setRollbackOnly();
      });
      eq("[1, 2, 4]", jdbc.queryForList("select id from acct order by id", Integer.class));
    });
    check("tx::readOnly", () -> {
      var ro = new TransactionTemplate(tm); ro.setReadOnly(true);
      eq("on", ro.execute(s -> jdbc.queryForObject("show transaction_read_only", String.class)));
      raises(DataAccessException.class, () -> ro.executeWithoutResult(s -> jdbc.update("delete from acct")));
      eq(3, jdbc.queryForObject("select count(*) from acct", Integer.class));
    });
    check("tx::isolation", () -> {
      var ser = new TransactionTemplate(tm); ser.setIsolationLevel(TransactionDefinition.ISOLATION_SERIALIZABLE);
      eq("serializable", ser.execute(s -> jdbc.queryForObject("show transaction_isolation", String.class)));
      var rr = new TransactionTemplate(tm); rr.setIsolationLevel(TransactionDefinition.ISOLATION_REPEATABLE_READ);
      eq("repeatable read", rr.execute(s -> jdbc.queryForObject("show transaction_isolation", String.class)));
      eq("read committed", jdbc.queryForObject("show transaction_isolation", String.class));
    });
    check("err::duplicateKey", () -> raises(DuplicateKeyException.class, () -> jdbc.update("insert into acct values (1, 0)")));
    check("err::foreignKey", () -> raises(DataIntegrityViolationException.class,
        () -> jdbc.update("insert into pet (owner_id, kind) values (999, 'x')")));
    check("err::notNull_check", () -> {
      raises(DataIntegrityViolationException.class, () -> jdbc.update("insert into acct values (9, null)"));
      raises(DataIntegrityViolationException.class, () -> jdbc.update("insert into pet (owner_id, kind) values (1, '')"));
    });
    check("err::badGrammar", () -> {
      var e = raises(BadSqlGrammarException.class, () -> jdbc.queryForList("select * from no_such_table"));
      eq("42P01", e.getSQLException().getSQLState());
      raises(BadSqlGrammarException.class, () -> jdbc.execute("selec 1"));
    });
    check("err::queryTimeout", () -> {
      var t = new JdbcTemplate(ds); t.setQueryTimeout(1);
      long t0 = System.nanoTime();
      raises(QueryTimeoutException.class, () -> t.execute("select pg_sleep(10)"));
      ok(System.nanoTime() - t0 < 6_000_000_000L, "cancel took too long");
      eq(1, jdbc.queryForObject("select 1", Integer.class));
    });
    check("lock::timeout_and_skipLocked", () -> {
      var other = Executors.newSingleThreadExecutor();
      try {
        tt.executeWithoutResult(s -> {
          jdbc.queryForList("select id from acct where id = 1 for update");
          try {
            var lt = new TransactionTemplate(tm); lt.setPropagationBehavior(TransactionDefinition.PROPAGATION_REQUIRES_NEW);
            String r = other.submit(() -> lt.execute(n -> {
              jdbc.execute("set local lock_timeout = '200ms'");
              String a = jdbc.queryForList("select id from acct order by id for update skip locked", Integer.class).toString();
              try { jdbc.queryForList("select id from acct where id = 1 for update"); return a + " no error"; }
              catch (DataAccessException e) { return a + " " + ((java.sql.SQLException) e.getMostSpecificCause()).getSQLState(); }
            })).get(20, TimeUnit.SECONDS);
            eq("[2, 4] 55P03", r);
          } catch (Exception e) { throw new RuntimeException(e); }
        });
      } finally { other.shutdownNow(); }
    });
    check("lock::serialization_failure", () -> {
      var ser = new TransactionTemplate(tm); ser.setIsolationLevel(TransactionDefinition.ISOLATION_SERIALIZABLE);
      ser.setPropagationBehavior(TransactionDefinition.PROPAGATION_REQUIRES_NEW);
      var other = Executors.newSingleThreadExecutor();
      try {
        raises(PessimisticLockingFailureException.class, () -> ser.executeWithoutResult(s -> {
          jdbc.queryForObject("select sum(bal) from acct", Integer.class);
          try {
            other.submit(() -> ser.executeWithoutResult(n -> {
              jdbc.queryForObject("select sum(bal) from acct", Integer.class);
              jdbc.update("update acct set bal = bal + 1 where id = 2");
            })).get(20, TimeUnit.SECONDS);
          } catch (Exception e) { throw new RuntimeException(e); }
          jdbc.update("update acct set bal = bal + 1 where id = 2");
        }));
      } finally { other.shutdownNow(); }
    });
  }
}
EOF
sed -i 's/JdbcChecks2.run(ctx);/JdbcChecks2.run(ctx);\n        JdbcChecks3.run(ctx);/' src/main/java/t/Main.java
mkdir -p flyway/sql flyway/broken flyway/legacy
cat > flyway/sql/V1__types_tables.sql <<'EOF'
-- enums, domains, a composite type, a sequence, tables with every constraint kind
create type status as enum ('new', 'paid', 'shipped');
create domain email as text check (value ~ '^[^@]+@[^@]+$');
create type money_pair as (amount numeric(12,2), currency char(3));
create sequence invoice_no start 1000 increment 5;
create table customer (
  id bigint generated by default as identity primary key,
  email email not null unique,
  name text not null,
  meta jsonb not null default '{}',
  created_at timestamptz not null default now(),
  updated_at timestamptz
);
create table orders (
  id bigserial primary key,
  customer_id bigint not null references customer(id) on delete cascade,
  status status not null default 'new',
  total money_pair,
  invoice int default nextval('invoice_no'),
  qty int not null check (qty > 0),
  price numeric(10,2) not null,
  amount numeric(12,2) generated always as (qty * price) stored,
  constraint orders_invoice_uq unique (invoice) deferrable initially deferred
);
create index orders_open_idx on orders (customer_id) where status <> 'shipped';
create index customer_lower_email on customer (lower(email));
create index customer_meta_gin on customer using gin (meta jsonb_path_ops);
comment on table customer is 'people who buy';
comment on column orders.amount is 'qty * price';
create table ${audit}.log (id bigserial primary key, tbl text, op text, at timestamptz default now(), row_data jsonb);
EOF
cat > flyway/sql/V2__functions.sql <<'EOF'
create function touch_updated() returns trigger language plpgsql as $$
begin
  new.updated_at := now(); -- a comment; with a semicolon
  return new;
end;
$$;
create trigger customer_touch before update on customer for each row execute function touch_updated();
create function ${audit}.record() returns trigger language plpgsql security definer as $body$
begin
  insert into ${audit}.log (tbl, op, row_data) values (tg_table_name, tg_op, to_jsonb(coalesce(new, old)));
  return null;
end
$body$;
create trigger orders_audit after insert or update or delete on orders
  for each row execute function ${audit}.record();
create function order_total(cid bigint) returns numeric language sql stable
  as 'select coalesce(sum(amount), 0) from ${flyway:defaultSchema}.orders where customer_id = cid';
create procedure ship(oid bigint) language plpgsql as $$
begin update ${flyway:defaultSchema}.orders set status = 'shipped' where id = oid; end $$;
create function sum_sq_step(s numeric, v numeric) returns numeric language sql immutable as 'select s + v * v';
create aggregate sum_sq(numeric) (sfunc = sum_sq_step, stype = numeric, initcond = '0');
EOF
cat > flyway/sql/V3__views_partitions.sql <<'EOF'
create view customer_totals as
  select c.id, c.name, count(o.id) as orders, coalesce(sum(o.amount), 0) as spent
  from customer c left join orders o on o.customer_id = c.id group by c.id, c.name;
create materialized view status_counts as select status, count(*) as n from orders group by status;
create unique index status_counts_uq on status_counts (status);
create table event (id bigint not null, at date not null, payload text) partition by range (at);
create table event_2024 partition of event for values from ('2024-01-01') to ('2025-01-01');
create table event_2025 partition of event for values from ('2025-01-01') to ('2026-01-01');
create table event_default partition of event default;
EOF
cat > flyway/sql/V3.1__data.sql <<'EOF'
insert into customer (email, name, meta) values ('a@x.io', 'Ann', '{"region": "${region}"}'), ('b@x.io', 'Bob', '{}');
insert into orders (customer_id, qty, price) values (1, 2, 9.99), (1, 1, 100), (2, 5, 1.10);
insert into event values (1, '2024-06-01', 'a'), (2, '2025-02-03', 'b'), (3, '2030-01-01', 'c');
EOF
cat > flyway/sql/V4__alter.sql <<'EOF'
alter type status add value 'refunded' after 'shipped';
alter table customer add column phone varchar(32);
alter table customer rename column name to full_name;
alter domain email set not null;
create or replace view customer_totals as
  select c.id, c.full_name as name, count(o.id) as orders, coalesce(sum(o.amount), 0) as spent
  from customer c left join orders o on o.customer_id = c.id group by c.id, c.full_name;
grant select on all tables in schema ${flyway:defaultSchema} to public;
EOF
cat > flyway/sql/V5__concurrent_index.sql <<'EOF'
create index concurrently orders_status_idx on orders (status);
EOF
cat > flyway/sql/R__report.sql <<'EOF'
create or replace view report as
  select ct.name, ct.spent, rank() over (order by ct.spent desc) as rnk from customer_totals ct;
EOF
cat > flyway/broken/V6__broken.sql <<'EOF'
create table half_done (id int);
insert into half_done values (1);
select no_such_function();
EOF
cat > flyway/legacy/V1__old.sql <<'EOF'
create table old_thing (id int);
EOF
cat > flyway/legacy/V2__new.sql <<'EOF'
alter table old_thing add column label text;
create view old_view as select * from old_thing;
EOF
cat > src/main/java/t/FlywayChecks.java <<'EOF'
package t;

import static t.T.*;

import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import org.flywaydb.core.Flyway;
import org.flywaydb.core.api.*;
import org.flywaydb.core.api.output.*;
import org.flywaydb.core.api.configuration.FluentConfiguration;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.jdbc.datasource.DriverManagerDataSource;

/** Flyway migrate / info / validate / repair / clean / baseline. */
final class FlywayChecks {
  static String dir;
  static FluentConfiguration conf(String... locs) {
    String[] l = Arrays.stream(locs).map(x -> "filesystem:" + dir + "/flyway/" + x).toArray(String[]::new);
    FluentConfiguration c = Flyway.configure().dataSource(T.url("flyway_test"), "postgres", "pw").schemas("app", "audit")
        .locations(l).placeholders(Map.of("region", "eu", "audit", "audit")).cleanDisabled(false);
    // a transaction-scoped advisory lock would make CREATE INDEX CONCURRENTLY wait for it forever
    c.getConfigurationExtension(org.flywaydb.database.postgresql.PostgreSQLConfigurationExtension.class).setTransactionalLock(false);
    return c;
  }
  static String objects(JdbcTemplate j, String schema) {
    return j.queryForObject("select (select count(*) from pg_class c join pg_namespace n on n.oid = c.relnamespace where nspname = ?)"
        + " || '/' || (select count(*) from pg_type t join pg_namespace n on n.oid = t.typnamespace where nspname = ?)"
        + " || '/' || (select count(*) from pg_proc p join pg_namespace n on n.oid = p.pronamespace where nspname = ?)",
        String.class, schema, schema, schema);
  }

  static void run(String d) throws Exception {
    dir = d;
    var j = new JdbcTemplate(new DriverManagerDataSource(T.url("flyway_test"), "postgres", "pw"));
    Flyway fw = conf("sql").load();
    check("migrate::fresh", () -> {
      MigrateResult r = fw.migrate();
      ok(r.success, "not success"); eq(7, r.migrationsExecuted); eq("5", r.targetSchemaVersion);
    });
    check("info::states", () -> {
      StringBuilder sb = new StringBuilder();
      for (MigrationInfo m : fw.info().all()) sb.append(m.getVersion()).append(':').append(m.getState()).append(' ');
      eq("<< Flyway Schema Creation >>:SUCCESS 1:SUCCESS 2:SUCCESS 3:SUCCESS 3.1:SUCCESS 4:SUCCESS 5:SUCCESS null:SUCCESS ",
          sb.toString().replace("null:SUCCESS 1:", "<< Flyway Schema Creation >>:SUCCESS 1:"));
      eq("5", fw.info().current().getVersion());
    });
    check("objects::triggers_views", () -> {
      j.update("update app.customer set phone = '1' where id = 1");
      ok(j.queryForObject("select updated_at is not null from app.customer where id = 1", Boolean.class), "trigger");
      eq(3, j.queryForObject("select count(*) from audit.log where op = 'INSERT'", Integer.class));
      eq("[Ann:119.98:1, Bob:5.50:2]", j.query("select * from app.report order by rnk", (rs, i) ->
          rs.getString(1) + ":" + rs.getBigDecimal(2) + ":" + rs.getInt(3)));
      j.execute("call app.ship(1)");
      j.execute("refresh materialized view concurrently app.status_counts");
      eq("[new=2, shipped=1]", j.query("select status, n from app.status_counts order by status", (rs, i) -> rs.getString(1) + "=" + rs.getInt(2)));
      eq("event_2024 event_2025 event_default", j.queryForObject(
          "select string_agg(tableoid::regclass::text, ' ' order by id) from app.event", String.class).replace("app.", ""));
      eq("{new,paid,shipped,refunded}", j.queryForObject("select enum_range(null::app.status)::text", String.class));
      eq("301015", j.queryForObject("select app.sum_sq(qty) || '' || nextval('app.invoice_no') from app.orders", String.class));
      eq("ok", j.queryForObject("select case when indisvalid then 'ok' end from pg_index where indexrelid = 'app.orders_status_idx'::regclass", String.class));
    });
    check("validate::ok", () -> ok(fw.validateWithResult().validationSuccessful, "validate"));
    check("migrate::noop", () -> eq(0, fw.migrate().migrationsExecuted));
    check("migrate::failing_rolls_back", () -> {
      raises(FlywayException.class, () -> conf("sql", "broken").load().migrate());
      eq(0, j.queryForObject("select count(*) from pg_tables where tablename = 'half_done'", Integer.class));
      eq("5", fw.info().current().getVersion());
      eq(0, j.queryForObject("select count(*) from app.flyway_schema_history where not success", Integer.class));
    });
    check("validate::checksum_then_repair", () -> {
      Path v2 = Path.of(dir, "flyway/sql/V2__functions.sql");
      Files.writeString(v2, Files.readString(v2) + "\n-- edited\n");
      var vr = fw.validateWithResult();
      ok(!vr.validationSuccessful, "validate should fail");
      ok(vr.getAllErrorMessages().contains("checksum"), vr.getAllErrorMessages());
      RepairResult rr = fw.repair();
      eq(1, rr.migrationsAligned.size());
      ok(fw.validateWithResult().validationSuccessful, "validate after repair");
    });
    check("migrate::concurrent", () -> {
      var pool = Executors.newFixedThreadPool(3);
      try {
        List<Future<Integer>> fs = new ArrayList<>();
        for (int i = 0; i < 3; i++)
          fs.add(pool.submit(() -> conf("sql").schemas("conc", "audit2").placeholders(Map.of("region", "us", "audit", "audit2"))
              .load().migrate().migrationsExecuted));
        int sum = 0; for (var f : fs) sum += f.get(120, TimeUnit.SECONDS);
        eq(7, sum);
        eq(8, j.queryForObject("select count(*) from conc.flyway_schema_history where success", Integer.class));
      } finally { pool.shutdownNow(); }
    });
    check("baseline::existing_schema", () -> {
      j.execute("create schema legacy");
      j.execute("create table legacy.old_thing (id int)");
      Flyway lf = Flyway.configure().dataSource(T.url("flyway_test"), "postgres", "pw").schemas("legacy")
          .locations("filesystem:" + dir + "/flyway/legacy").baselineOnMigrate(true).baselineVersion("1").cleanDisabled(false).load();
      eq(1, lf.migrate().migrationsExecuted);
      eq("1:BASELINE_IGNORED 1:BASELINE 2:SUCCESS", Arrays.stream(lf.info().all()).map(m -> m.getVersion() + ":" + m.getState()).reduce((a, b) -> a + " " + b).get());
    });
    check("clean::existing_schema", () -> {
      Flyway lf = Flyway.configure().dataSource(T.url("flyway_test"), "postgres", "pw").schemas("legacy")
          .locations("filesystem:" + dir + "/flyway/legacy").cleanDisabled(false).load();
      lf.clean();
      eq("0/0/0", objects(j, "legacy"));
      eq(1, j.queryForObject("select count(*) from pg_namespace where nspname = 'legacy'", Integer.class));
    });
    check("clean::drops_schemas", () -> {
      CleanResult cr = fw.clean();
      ok(cr.schemasDropped.containsAll(List.of("app", "audit")), "dropped " + cr.schemasDropped + " cleaned " + cr.schemasCleaned);
      eq(0, j.queryForObject("select count(*) from pg_namespace where nspname in ('app', 'audit')", Integer.class));
    });
    check("clean::schema_not_created_by_flyway", () -> {
      Flyway cf = conf("sql").schemas("conc", "audit2").placeholders(Map.of("region", "us", "audit", "audit2")).load();
      cf.clean();
      eq("0/0/0 0/0/0", objects(j, "conc") + " " + objects(j, "audit2"));
    });
    check("migrate::after_clean", () -> {
      Files.writeString(Path.of(dir, "flyway/sql/V2__functions.sql"),
          Files.readString(Path.of(dir, "flyway/sql/V2__functions.sql")).replace("\n-- edited\n", ""));
      eq(7, fw.migrate().migrationsExecuted);
      ok(fw.validateWithResult().validationSuccessful, "validate");
    });
  }
}
EOF
cat > lb/main.xml <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<databaseChangeLog xmlns="http://www.liquibase.org/xml/ns/dbchangelog"
    xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
    xsi:schemaLocation="http://www.liquibase.org/xml/ns/dbchangelog http://www.liquibase.org/xml/ns/dbchangelog/dbchangelog-latest.xsd">
  <changeSet id="1" author="t">
    <createTable tableName="author" remarks="writers">
      <column name="id" type="bigint" autoIncrement="true"><constraints primaryKey="true" primaryKeyName="author_pk"/></column>
      <column name="name" type="varchar(100)"><constraints nullable="false"/></column>
      <column name="bio" type="text"/>
      <column name="born" type="date"/>
    </createTable>
  </changeSet>
  <changeSet id="2" author="t">
    <createTable tableName="book">
      <column name="id" type="int" autoIncrement="true"><constraints primaryKey="true"/></column>
      <column name="author_id" type="bigint"><constraints nullable="false"/></column>
      <column name="title" type="varchar(200)"/>
      <column name="price" type="numeric(8,2)" defaultValueNumeric="0"/>
      <column name="published" type="boolean" defaultValueBoolean="false"/>
      <column name="tags" type="text[]"/>
      <column name="info" type="jsonb"/>
      <column name="created" type="timestamptz" defaultValueComputed="now()"/>
    </createTable>
    <addForeignKeyConstraint baseTableName="book" baseColumnNames="author_id" constraintName="book_author_fk"
        referencedTableName="author" referencedColumnNames="id" onDelete="CASCADE"/>
    <createIndex tableName="book" indexName="book_title_idx"><column name="title"/></createIndex>
    <addUniqueConstraint tableName="book" columnNames="author_id, title" constraintName="book_author_title_uq"/>
  </changeSet>
  <changeSet id="3" author="t">
    <createSequence sequenceName="isbn_seq" startValue="100" incrementBy="10" maxValue="100000" cycle="true"/>
    <createView viewName="book_list">select b.id, b.title, a.name as author from book b join author a on a.id = b.author_id</createView>
  </changeSet>
  <changeSet id="4" author="t">
    <sql splitStatements="false">
      create function book_touch() returns trigger language plpgsql as $$
      begin new.title := trim(new.title); return new; end; $$;
      create trigger book_touch_trg before insert or update on book for each row execute function book_touch();
    </sql>
    <rollback>drop trigger book_touch_trg on book; drop function book_touch();</rollback>
  </changeSet>
  <changeSet id="5" author="t"><tagDatabase tag="v1"/></changeSet>
  <changeSet id="6" author="t">
    <addColumn tableName="book"><column name="rating" type="smallint"/></addColumn>
    <addDefaultValue tableName="book" columnName="rating" defaultValueNumeric="3"/>
    <renameColumn tableName="author" oldColumnName="bio" newColumnName="biography"/>
    <addNotNullConstraint tableName="book" columnName="title" defaultNullValue="untitled"/>
    <createIndex tableName="book" indexName="book_rating_idx" unique="false"><column name="rating"/><column name="price" descending="true"/></createIndex>
  </changeSet>
  <changeSet id="7" author="t">
    <insert tableName="author"><column name="id" valueNumeric="1"/><column name="name" value="Le Guin"/><column name="born" valueDate="1929-10-21"/></insert>
    <insert tableName="book"><column name="id" valueNumeric="1"/><column name="author_id" valueNumeric="1"/><column name="title" value="  Lathe "/><column name="price" valueNumeric="12.5"/></insert>
    <update tableName="book"><column name="published" valueBoolean="true"/><where>id = 1</where></update>
    <rollback>delete from book; delete from author;</rollback>
  </changeSet>
  <changeSet id="8" author="t"><tagDatabase tag="v2"/></changeSet>
  <include file="lb/more.sql"/>
</databaseChangeLog>
EOF
cat > lb/more.sql <<'EOF'
--liquibase formatted sql

--changeset t:9
create type genre as enum ('scifi', 'fantasy', 'other');
alter table book add column genre genre default 'other';
--rollback alter table book drop column genre; drop type genre;

--changeset t:10
create table review (id bigint generated always as identity primary key, book_id int references book(id), stars int check (stars between 1 and 5), body text);
create index review_book_idx on review (book_id) where stars >= 4;
comment on column review.stars is 'one to five';
--rollback drop table review;

--changeset t:11 runOnChange:true
create or replace view top_books as select title, avg(stars) as avg_stars from book join review on review.book_id = book.id group by title;
--rollback drop view top_books;
EOF
cat > src/main/java/t/LiquibaseChecks.java <<'EOF'
package t;

import static t.T.*;

import java.io.*;
import java.nio.file.*;
import java.util.*;
import java.util.regex.*;
import liquibase.Scope;
import liquibase.command.CommandScope;
import liquibase.resource.DirectoryResourceAccessor;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.jdbc.datasource.DriverManagerDataSource;

/** Liquibase commands through CommandScope; snapshot / generateChangelog hashed per object. */
final class LiquibaseChecks {
  static String dir;

  /** Runs a command against db; returns its text output. */
  static String cmd(String db, String name, Object... args) throws Exception {
    var out = new ByteArrayOutputStream();
    Map<String, Object> scope = new HashMap<>();
    scope.put(Scope.Attr.resourceAccessor.name(), new DirectoryResourceAccessor(new File(dir)));
    Scope.child(scope, () -> {
      var c = new CommandScope(name).addArgumentValue("url", T.url(db))
          .addArgumentValue("username", "postgres").addArgumentValue("password", "pw").setOutput(out);
      for (int i = 0; i < args.length; i += 2) c.addArgumentValue((String) args[i], args[i + 1]);
      c.execute();
    });
    return out.toString();
  }
  static JdbcTemplate jt(String db) { return new JdbcTemplate(new DriverManagerDataSource(T.url(db), "postgres", "pw")); }
  static List<String> cols(JdbcTemplate j, String t) {
    return j.queryForList("select column_name from information_schema.columns where table_schema = 'public' and table_name = ? order by ordinal_position", String.class, t);
  }
  /** Diff report minus the lines naming the two databases. */
  static String norm(String diff) {
    StringBuilder sb = new StringBuilder();
    for (String l : diff.split("\n"))
      if (!l.matches("(?i).*(jdbc:|database:|product (name|version)|liquibase_(test|ref|gen)).*")) sb.append(l).append('\n');
    return sb.toString();
  }

  static void run(String d) throws Exception {
    dir = d;
    var j = jt("liquibase_test");
    String cl = "lb/main.xml";
    check("update", () -> {
      cmd("liquibase_test", "update", "changelogFile", cl);
      eq(11, j.queryForObject("select count(*) from databasechangelog", Integer.class));
      eq("[1:Lathe:true:3]", j.query("select id, title, published, rating from book", (rs, i) ->
          rs.getInt(1) + ":" + rs.getString(2) + ":" + rs.getBoolean(3) + ":" + rs.getInt(4)));
    });
    check("status_uptodate", () -> ok(cmd("liquibase_test", "status", "changelogFile", cl, "verbose", true).contains("is up to date"), "status"));
    check("history", () -> { String h = cmd("liquibase_test", "history"); save("liquibase-history.txt", h);
      eq(11, h.lines().filter(l -> l.contains("lb/m") || l.contains("lb/more")).count()); });
    check("validate", () -> cmd("liquibase_test", "validate", "changelogFile", cl));
    check("tagExists", () -> cmd("liquibase_test", "tagExists", "tag", "v2"));
    check("rollback_to_tag", () -> {
      cmd("liquibase_test", "rollback", "changelogFile", cl, "tag", "v1");
      eq("[id, author_id, title, price, published, tags, info, created]", cols(j, "book"));
      eq("[id, name, bio, born]", cols(j, "author"));
      eq(0, j.queryForObject("select count(*) from pg_type where typname = 'genre'", Integer.class));
      eq(4, j.queryForObject("select count(*) from databasechangelog", Integer.class));
    });
    check("updateToTag_then_update", () -> {
      cmd("liquibase_test", "updateToTag", "changelogFile", cl, "tag", "v2");
      eq(8, j.queryForObject("select count(*) from databasechangelog", Integer.class));
      cmd("liquibase_test", "update", "changelogFile", cl);
      eq(11, j.queryForObject("select count(*) from databasechangelog", Integer.class));
    });
    check("rollbackCount", () -> {
      cmd("liquibase_test", "rollbackCount", "changelogFile", cl, "count", 2);
      eq(0, j.queryForObject("select count(*) from pg_class where relname in ('review', 'top_books')", Integer.class));
      cmd("liquibase_test", "update", "changelogFile", cl);
    });
    check("sql_previews", () -> {
      String fr = cmd("liquibase_test", "futureRollbackSql", "changelogFile", cl);
      ok(fr.contains("SQL to roll back currently unexecuted changes"), fr);
      String rs = cmd("liquibase_test", "rollbackCountSql", "changelogFile", cl, "count", 3);
      ok(rs.contains("drop table review"), rs);
      ok(cmd("liquibase_test", "updateSql", "changelogFile", cl).contains("databasechangeloglock"), "updateSql");
    });
    check("checksums", () -> {
      cmd("liquibase_test", "clearChecksums");
      eq(11, j.queryForObject("select count(*) from databasechangelog where md5sum is null", Integer.class));
      cmd("liquibase_test", "update", "changelogFile", cl);
      eq(0, j.queryForObject("select count(*) from databasechangelog where md5sum is null", Integer.class));
      ok(cmd("liquibase_test", "calculateChecksum", "changelogFile", cl, "changesetIdentifier", "lb/main.xml::2::t").contains(":"), "checksum");
    });
    check("locks", () -> {
      j.update("update databasechangeloglock set locked = true, lockgranted = now(), lockedby = 'someone' where id = 1");
      ok(cmd("liquibase_test", "listLocks").contains("someone"), "listLocks");
      cmd("liquibase_test", "releaseLocks");
      eq(false, j.queryForObject("select locked from databasechangeloglock", Boolean.class));
    });
    LiquibaseSnap.run(j);
    LiquibaseDiff.run(cl);
  }
}
EOF
cat > src/main/java/t/LiquibaseSnap.java <<'EOF'
package t;

import static t.T.*;

import java.sql.DriverManager;
import java.util.*;
import liquibase.CatalogAndSchema;
import liquibase.database.*;
import liquibase.database.jvm.JdbcConnection;
import liquibase.snapshot.*;
import liquibase.structure.DatabaseObject;
import liquibase.structure.core.*;
import org.springframework.jdbc.core.JdbcTemplate;

/** A Liquibase snapshot of public, one hashed id per object (all its attributes). */
final class LiquibaseSnap {
  static String val(Object v) {
    if (v == null) return "null";
    if (v instanceof DatabaseObject o) return o.getClass().getSimpleName() + ":" + o.getName();
    if (v instanceof Collection<?> c) { List<String> l = new ArrayList<>(); for (Object x : c) l.add(val(x)); return l.toString(); }
    return v.toString();
  }
  static String describe(DatabaseObject o) {
    StringBuilder sb = new StringBuilder();
    for (String a : new TreeSet<>(o.getAttributes())) {
      if (a.equals("snapshotId")) continue;
      sb.append(a).append('=').append(val(o.getAttribute(a, Object.class))).append('\n');
    }
    return sb.toString();
  }
  static Database open(String db) throws Exception {
    return DatabaseFactory.getInstance().findCorrectDatabaseImplementation(
        new JdbcConnection(DriverManager.getConnection(T.url(db), "postgres", "pw")));
  }

  /** Snapshots only the given kinds (so one failing catalog query fails only its kind). */
  @SafeVarargs
  static void snap(String id, Class<? extends DatabaseObject>... kinds) {
    check(id, () -> {
      Database db = open("liquibase_test");
      DatabaseSnapshot s = SnapshotGeneratorFactory.getInstance().createSnapshot(
          new CatalogAndSchema[] {new CatalogAndSchema(null, "public")}, db, new SnapshotControl(db, kinds));
      StringBuilder all = new StringBuilder();
      int n = 0;
      for (var k : kinds) {
        if (k.equals(Schema.class)) continue;
        List<String> items = new ArrayList<>();
        for (DatabaseObject o : s.get(k)) {
          String rel = o instanceof Column c && c.getRelation() != null ? c.getRelation().getName() + "." : "";
          String text = describe(o);
          if (o instanceof View v) text = text.replaceAll("\\s+", " ");
          items.add(k.getSimpleName() + ":" + rel + o.getName() + "\t" + text);
        }
        Collections.sort(items);
        for (String it : items) {
          String[] p = it.split("\t", 2);
          if (!k.equals(Table.class) || kinds.length == 3) blob(id + "::" + p[0], p[1]);
          all.append("== ").append(p[0]).append('\n').append(p[1]);
          n++;
        }
      }
      save("liquibase-" + id.replace("::", "-") + ".txt", all.toString());
      ok(n > 0, "no objects");
      db.close();
    });
  }

  static void run(JdbcTemplate j) {
    snap("snapshot::tables_views", Schema.class, Table.class, View.class);
    snap("snapshot::columns", Schema.class, Table.class, View.class, Column.class);
    snap("snapshot::keys_indexes", Schema.class, Table.class, Index.class, PrimaryKey.class, ForeignKey.class, UniqueConstraint.class);
    snap("snapshot::sequences", Schema.class, Sequence.class);
    check("snapshot::command_json", () -> {
      String js = LiquibaseChecks.cmd("liquibase_test", "snapshot", "snapshotFormat", "json");
      save("liquibase-snapshot.json", js);
      ok(js.contains("\"book_author_fk\"") && js.contains("\"isbn_seq\""), "json snapshot lacks objects");
    });
    check("generateChangelog", () -> {
      java.nio.file.Path out = java.nio.file.Path.of(LiquibaseChecks.dir, "lb/generated.xml");
      java.nio.file.Files.deleteIfExists(out);
      LiquibaseChecks.cmd("liquibase_test", "generateChangelog", "changelogFile", "lb/generated.xml",
          "diffTypes", "tables,columns,views,indexes,foreignkeys,primarykeys,uniqueconstraints,sequences,data");
      String xml = java.nio.file.Files.readString(out);
      save("liquibase-generated.xml", xml);
      var m = java.util.regex.Pattern.compile("(?s)<changeSet [^>]*>(.*?)</changeSet>").matcher(xml);
      List<String> sets = new ArrayList<>();
      // now() defaults differ per run
      while (m.find()) sets.add(m.group(1).replaceAll("\\s+", " ").replaceAll("valueDate=\"\\d{4}-\\d\\d-\\d\\dT[^\"]*\"", "valueDate=\"T\"").trim());
      Collections.sort(sets);
      Map<String, Integer> seen = new HashMap<>();
      for (String cs : sets) {
        String kind = cs.replaceAll("^<([A-Za-z]+).*?(?:(?:tableName|viewName|sequenceName|indexName|constraintName)=\"([^\"]*)\")?.*$", "$1:$2");
        int k = seen.merge(kind, 1, Integer::sum);
        blob("gen::" + kind + (k > 1 ? "#" + k : ""), cs);
      }
      ok(sets.size() > 10, "only " + sets.size() + " changeSets");
    });
  }
}
EOF
cat > src/main/java/t/LiquibaseDiff.java <<'EOF'
package t;

import static t.T.*;
import static t.LiquibaseChecks.*;

import java.nio.file.*;
import java.util.*;

/** diff / diffChangelog between databases, the generated changelog's round trip, dbDoc, dropAll. */
final class LiquibaseDiff {
  static String diff(String target, String ref) throws Exception {
    return norm(cmd(target, "diff", "referenceUrl", T.url(ref), "referenceUsername", "postgres", "referencePassword", "pw"));
  }
  /** One hashed id per diff section ("Missing Index(s): ..." and the lines under it). */
  static void sections(String id, String report) {
    String cur = null; StringBuilder sb = new StringBuilder();
    for (String l : (report + "\nEND:\n").split("\n")) {
      if (!l.startsWith(" ") && l.contains(":")) {
        if (cur != null) blob(id + "::" + cur, sb.toString());
        cur = l.substring(0, l.indexOf(':')).trim(); sb.setLength(0);
      }
      sb.append(l.trim()).append('\n');
    }
  }

  static void run(String cl) {
    var ref = jt("liquibase_ref");
    check("diff::against_v1", () -> {
      cmd("liquibase_ref", "updateToTag", "changelogFile", cl, "tag", "v1");
      // Liquibase's open-source edition neither diffs nor generates enum types
      ref.execute("create type genre as enum ('scifi', 'fantasy', 'other')");
      jt("liquibase_gen").execute("create type genre as enum ('scifi', 'fantasy', 'other')");
      String d = diff("liquibase_test", "liquibase_ref");
      save("liquibase-diff-v1.txt", d);
      sections("diff_v1", d);
      ok(d.contains("book_rating_idx") && d.contains("review"), d);
    });
    check("diffChangelog::apply", () -> {
      Path out = Path.of(dir, "lb/diff.xml");
      Files.deleteIfExists(out);
      cmd("liquibase_ref", "diffChangelog", "changelogFile", "lb/diff.xml", "referenceUrl", T.url("liquibase_test"),
          "referenceUsername", "postgres", "referencePassword", "pw");
      String xml = Files.readString(out);
      save("liquibase-diff.xml", xml);
      var m = java.util.regex.Pattern.compile("(?s)<changeSet [^>]*>(.*?)</changeSet>").matcher(xml);
      List<String> sets = new ArrayList<>();
      while (m.find()) sets.add(m.group(1).replaceAll("\\s+", " ").trim());
      Collections.sort(sets);
      for (int i = 0; i < sets.size(); i++) blob("diffcl::" + i, sets.get(i));
      cmd("liquibase_ref", "update", "changelogFile", "lb/diff.xml");
      String after = diff("liquibase_ref", "liquibase_test");
      save("liquibase-diff-after.txt", after);
      sections("diff_after", after);
      eq(1, ref.queryForObject("select count(*) from pg_class where relname = 'review' and relnamespace = 'public'::regnamespace", Integer.class));
    });
    check("generated::roundtrip", () -> {
      cmd("liquibase_gen", "update", "changelogFile", "lb/generated.xml");
      eq(1, jt("liquibase_gen").queryForObject("select count(*) from book where title = 'Lathe'", Integer.class));
      String d = diff("liquibase_gen", "liquibase_test");
      save("liquibase-diff-roundtrip.txt", d);
      sections("roundtrip", d);
    });
    check("dbDoc", () -> {
      Path o = Path.of(dir, "lb/doc");
      cmd("liquibase_test", "dbDoc", "changelogFile", cl, "outputDirectory", o.toString());
      ok(Files.exists(o.resolve("index.html")), "no index.html");
      try (var s = Files.walk(o)) { ok(s.anyMatch(p -> p.toString().contains("book")), "no page for book"); }
    });
    check("dropAll", () -> {
      cmd("liquibase_test", "dropAll", "requireForce", false, "force", true);
      var j = jt("liquibase_test");
      eq(0, j.queryForObject("select count(*) from pg_class where relnamespace = 'public'::regnamespace", Integer.class));
    });
  }
}
EOF
$MVN -Dliquibase.analytics.enabled=false package dependency:copy-dependencies -DincludeScope=runtime \
  -Dmaven.test.skip=true > "$OUT/build.log" 2>&1 || { tail -40 "$OUT/build.log"; exit 1; }
CP="target/classes:target/dependency/*"
J="java -Dliquibase.analytics.enabled=false -Dliquibase.showBanner=false -Dliquibase.logLevel=WARNING -cp $CP t.Main"
for part in jdbc flyway liquibase; do
  echo "== $part"
  timeout 900 $J $part $W > "$OUT/$part.log" 2>&1 || echo "$part exited $?"
  grep -E '^(FAIL |[a-z]+:: pass)' "$OUT/$part.log" | cut -c1-400
done
