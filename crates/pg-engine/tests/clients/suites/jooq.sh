# jOOQ 3.21.9 (Java, open-source edition) with pgjdbc 42.7.14: jOOQ's PostgreSQL integration
# tests are not public, so a pinned driver program instead. Two Maven projects under
# /cache/work/jooq: gen/ makes the schema (shop, audit: enums, domains, composite types,
# ranges, inet, arrays, jsonb, identity / generated columns, partitions, inheritance, views,
# a materialized view, sequences, functions with OUT / table / array / enum / composite
# parameters, an overload, a procedure, an aggregate, triggers, comments, cross-schema FKs) and
# runs jooq-codegen's PostgresDatabase over it; app/ compiles the generated code and runs checks.
# One PASS/FAIL line each, in jooq.lst:
#   codegen::files     the generator ran; gen::<file> one id per generated file with a hash of
#                      its text, so a catalog read differently shows as engine=MISSING (the
#                      files are kept in $OUT/generated);
#   meta::*            runtime Meta over JDBC DatabaseMetaData (tables, keys, DDL export);
#   dsl::*, rec::*, batch::*, upsert::*, merge::*, array::*, json::*, range::*, enum::*,
#   udt::*, routine::*, tx::*   generated-code and DSL queries.
# Database: jooq.setup.sql (jooq_test). Maven 3.9.16 is the src tarball; repository /cache/home/m2.
# image: eclipse-temurin:25-jdk
# src: https://archive.apache.org/dist/maven/maven-3/3.9.16/binaries/apache-maven-3.9.16-bin.tar.gz
set -eu
MVN="$SRC/bin/mvn -B -q -Dmaven.repo.local=/cache/home/m2"
W=/cache/work/jooq
rm -rf $W && mkdir -p $W/gen/src/main/java/t $W/app/src/main/java/t && cd $W
pom() { # pom <artifactId> <extra dependency xml>
cat <<EOF
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>t</groupId><artifactId>$1</artifactId><version>1</version>
  <properties>
    <maven.compiler.release>25</maven.compiler.release>
    <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding>
  </properties>
  <dependencies>
    <dependency><groupId>org.jooq</groupId><artifactId>jooq</artifactId><version>3.21.9</version></dependency>
    <dependency><groupId>org.jooq</groupId><artifactId>jooq-postgres-extensions</artifactId><version>3.21.9</version></dependency>
    <dependency><groupId>org.postgresql</groupId><artifactId>postgresql</artifactId><version>42.7.14</version></dependency>
    <dependency><groupId>org.slf4j</groupId><artifactId>slf4j-simple</artifactId><version>2.0.17</version></dependency>
    $2
  </dependencies>
  <build><plugins>
    <plugin><groupId>org.apache.maven.plugins</groupId><artifactId>maven-compiler-plugin</artifactId><version>3.15.0</version></plugin>
    <plugin><groupId>org.apache.maven.plugins</groupId><artifactId>maven-dependency-plugin</artifactId><version>3.11.0</version></plugin>
  </plugins></build>
</project>
EOF
}
pom jooq-gen '<dependency><groupId>org.jooq</groupId><artifactId>jooq-codegen</artifactId><version>3.21.9</version></dependency>' > gen/pom.xml
pom jooq-app '' > app/pom.xml
cat > gen/schema.sql <<'EOF'
create schema shop;
create schema audit;
set search_path = shop;
create type mood as enum ('sad', 'ok', 'happy');
create domain posint as integer check (value > 0);
create domain price as numeric(10,2) not null default 0 check (value >= 0);
create type address as (street text, city text, zip varchar(10));
create sequence invoice_seq start 1000 increment 10;
create table customer (
  id int generated always as identity primary key,
  name text not null,
  email varchar(200) unique,
  mood mood default 'ok',
  home address,
  tags text[] not null default '{}',
  scores int[],
  prefs jsonb,
  raw json,
  ip inet,
  active_during tstzrange,
  born date,
  member_since daterange,
  level_range int4range,
  uid uuid not null default gen_random_uuid(),
  created_at timestamptz not null default now(),
  name_upper text generated always as (upper(name)) stored
);
comment on table customer is 'Customers';
comment on column customer.prefs is 'free-form preferences';
create table product (id serial primary key, sku varchar(20) not null unique, title text, price price,
  dims int[], weight real, discontinued boolean not null default false,
  constraint sku_format check (sku ~ '^[A-Z]{3}-[0-9]+$'));
create table orders (
  id bigserial primary key,
  customer_id int not null references customer(id) on delete cascade,
  placed_at timestamp not null default localtimestamp,
  invoice int not null default nextval('invoice_seq'),
  qty posint,
  note text
);
create index orders_cust_placed on orders (customer_id, placed_at desc);
create unique index orders_invoice_uq on orders (invoice);
create index customer_lower_name on customer (lower(name)) where email is not null;
create table order_line (
  order_id bigint references orders(id) on delete cascade,
  line_no smallint,
  product_id int not null references product(id),
  qty int not null default 1,
  unit_price numeric(10,2),
  primary key (order_id, line_no)
);
create table shipment (order_id bigint, line_no smallint, shipped_on date,
  foreign key (order_id, line_no) references order_line on update cascade);
create table measurement (id bigint not null, taken date not null, v double precision) partition by range (taken);
create table measurement_2025 partition of measurement for values from ('2025-01-01') to ('2026-01-01');
create table measurement_rest partition of measurement default;
create table base_doc (id serial primary key, body text);
create table special_doc (stamp text) inherits (base_doc);
create view customer_summary as
  select c.id, c.name, count(o.id) as orders, coalesce(sum(o.qty), 0)::int as items
  from customer c left join orders o on o.customer_id = c.id group by c.id, c.name;
create materialized view product_stats as select discontinued, count(*) as n, avg(price) as avg_price from product group by discontinued;
create function order_count(cid int) returns bigint language sql stable as 'select count(*) from shop.orders where customer_id = cid';
create function split_name(full_name text, out first text, out last text) language plpgsql immutable as $$
begin first := split_part(full_name, ' ', 1); last := split_part(full_name, ' ', 2); end $$;
create function customers_with(m mood) returns table (id int, name text) language sql stable
  as 'select id, name from shop.customer where mood = m order by id';
create function bump_all(xs int[], by int default 1) returns int[] language sql immutable
  as 'select array_agg(x + by order by ord) from unnest(xs) with ordinality as t(x, ord)';
create function fmt(a address) returns text language sql immutable as 'select a.street || '', '' || a.city';
create function twice(x int) returns int language sql immutable as 'select x * 2';
create function twice(x text) returns text language sql immutable as 'select x || x';
create procedure rename_customer(cid int, new_name text, inout old_name text default null) language plpgsql as $$
begin select name into old_name from shop.customer where id = cid; update shop.customer set name = new_name where id = cid; end $$;
create function product_sum_sq(s numeric, v numeric) returns numeric language sql immutable as 'select s + v * v';
create aggregate sum_sq(numeric) (sfunc = product_sum_sq, stype = numeric, initcond = '0');
create table audit.event (id bigserial primary key, customer_id int references shop.customer(id) on delete set null,
  kind text not null, at timestamptz default now());
create function audit.on_order() returns trigger language plpgsql as $$
begin insert into audit.event (customer_id, kind) values (new.customer_id, 'order:' || tg_op); return new; end $$;
create trigger orders_audit after insert or update on orders for each row execute function audit.on_order();
EOF
cat > gen/src/main/java/t/Gen.java <<'EOF'
package t;

import java.nio.file.*;
import java.sql.*;
import org.jooq.codegen.GenerationTool;
import org.jooq.meta.jaxb.*;

/** Makes the schema, then runs jOOQ's code generator over it into the app project. */
public class Gen {
  static ForcedType ft(String type, String user, String binding) {
    return new ForcedType().withIncludeTypes(type).withUserType("org.jooq.postgres.extensions.types." + user)
        .withBinding("org.jooq.postgres.extensions.bindings." + binding);
  }
  public static void main(String[] a) throws Exception {
    String url = "jdbc:postgresql://" + System.getenv("PGHOST") + ":" + System.getenv("PGPORT") + "/jooq_test";
    try (Connection c = DriverManager.getConnection(url, "postgres", "pw"); Statement s = c.createStatement()) {
      s.execute("drop schema if exists shop, audit cascade");
      s.execute(Files.readString(Path.of(a[0])));
    }
    long t0 = System.nanoTime();
    GenerationTool.generate(new Configuration()
        .withLogging(Logging.WARN).withOnError(OnError.LOG)
        .withJdbc(new Jdbc().withDriver("org.postgresql.Driver").withUrl(url).withUser("postgres").withPassword("pw"))
        .withGenerator(new Generator()
            .withName("org.jooq.codegen.JavaGenerator")
            .withDatabase(new Database().withName("org.jooq.meta.postgres.PostgresDatabase")
                .withSchemata(new SchemaMappingType().withInputSchema("shop"), new SchemaMappingType().withInputSchema("audit"))
                .withForcedTypes(
                    ft("int4range", "IntegerRange", "IntegerRangeBinding"),
                    ft("daterange", "LocalDateRange", "LocalDateRangeBinding"),
                    ft("tstzrange", "OffsetDateTimeRange", "OffsetDateTimeRangeBinding"),
                    ft("inet", "Inet", "InetBinding")))
            .withGenerate(new Generate().withPojos(true).withPojosAsJavaRecordClasses(true).withDaos(false)
                .withGeneratedAnnotation(false).withComments(true).withIndexes(true).withSequences(true)
                .withRoutines(true).withUdts(true).withGlobalObjectReferences(true))
            .withTarget(new Target().withPackageName("gen").withDirectory(a[1]))));
    System.out.println("codegen ms " + (System.nanoTime() - t0) / 1_000_000);
  }
}
EOF
cat > app/src/main/java/t/T.java <<'EOF'
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
cat > app/src/main/java/t/Main.java <<'EOF'
package t;

import static t.T.*;

import java.nio.file.Files;
import java.nio.file.Path;
import java.sql.*;
import java.util.*;
import org.jooq.DSLContext;
import org.jooq.SQLDialect;
import org.jooq.impl.DSL;

public class Main {
  public static void main(String[] a) throws Exception {
    T.open("jooq");
    Path gen = Path.of(a[0]);
    check("codegen::files", () -> {
      List<Path> files;
      try (var s = Files.walk(gen)) { files = s.filter(p -> p.toString().endsWith(".java")).sorted().toList(); }
      for (Path p : files) blob("gen::" + gen.relativize(p), Files.readString(p));
      ok(files.size() > 60, "only " + files.size() + " files");
    });
    try (Connection c = DriverManager.getConnection(T.url("jooq_test"), "postgres", "pw")) {
      DSLContext ctx = DSL.using(c, SQLDialect.POSTGRES, new org.jooq.conf.Settings().withReturnAllOnUpdatableRecord(true));
      Checks.run(ctx);
      Checks2.run(ctx);
      Checks3.run(ctx);
    }
    T.close();
    System.exit(0);
  }
}
EOF
cat > app/src/main/java/t/Checks.java <<'EOF'
package t;

import static gen.shop.Tables.*;
import static org.jooq.impl.DSL.*;
import static t.T.*;

import gen.shop.enums.Mood;
import gen.shop.tables.records.*;
import gen.shop.udt.records.AddressRecord;
import java.math.BigDecimal;
import java.time.*;
import java.util.*;
import org.jooq.*;
import org.jooq.postgres.extensions.types.*;

/** Records, DAOs, inserts with RETURNING, enums, composite types, arrays, ranges, inet. */
final class Checks {
  static Mood mood(String s) { return EnumType.lookupLiteral(Mood.class, s); }
  static <V> V one(DSLContext ctx, TableField<?, V> f, Condition c) { return ctx.select(f).from(f.getTable()).where(c).fetchOne(f); }
  static <V> V one(DSLContext ctx, Table<?> t, Field<V> f, Condition c) { return ctx.select(f).from(t).where(c).fetchOne(f); }
  static void run(DSLContext ctx) {
    check("dsl::insert_returning", () -> {
      org.jooq.Record r = ctx.insertInto(CUSTOMER, CUSTOMER.NAME, CUSTOMER.EMAIL, CUSTOMER.MOOD, CUSTOMER.HOME, CUSTOMER.TAGS,
              CUSTOMER.SCORES, CUSTOMER.PREFS, CUSTOMER.RAW, CUSTOMER.IP, CUSTOMER.LEVEL_RANGE, CUSTOMER.MEMBER_SINCE, CUSTOMER.BORN)
          .values("ann lee", "ann@x.io", mood("happy"), new AddressRecord("1 Main St", "Oslo", "0150"), new String[] {"a", "b"},
              new Integer[] {3, 1, 2}, JSONB.valueOf("{\"theme\": \"dark\", \"n\": [1, 2]}"), JSON.valueOf("{\"k\":  1}"),
              Inet.inet(java.net.InetAddress.getByName("10.1.2.3"), 24), IntegerRange.integerRange(1, 10),
              LocalDateRange.localDateRange(LocalDate.of(2020, 1, 1), null), LocalDate.of(1990, 5, 6))
          .returning(CUSTOMER.ID, CUSTOMER.UID, CUSTOMER.NAME_UPPER, CUSTOMER.CREATED_AT).fetchOne();
      eq(1, r.get(CUSTOMER.ID)); eq("ANN LEE", r.get(CUSTOMER.NAME_UPPER)); ok(r.get(CUSTOMER.UID) != null, "uid");
    });
    check("rec::store_refresh_delete", () -> {
      CustomerRecord r = ctx.newRecord(CUSTOMER);
      r.setName("bob"); r.setMood(mood("sad")); r.setTags(new String[0]); r.store();
      eq(2, r.getId()); ok(r.getUid() != null, "uid after store"); eq("BOB", r.getNameUpper());
      r.setEmail("bob@x.io"); eq(1, r.store());
      ctx.update(CUSTOMER).set(CUSTOMER.NAME, "bobby").where(CUSTOMER.ID.eq(2)).execute();
      r.refresh(); eq("bobby", r.getName());
      CustomerRecord t = ctx.newRecord(CUSTOMER); t.setName("tmp"); t.setTags(new String[0]); t.store();
      eq(1, t.delete()); eq(2, ctx.fetchCount(CUSTOMER));
    });
    check("rec::fetch_types", () -> {
      CustomerRecord r = ctx.fetchSingle(CUSTOMER, CUSTOMER.ID.eq(1));
      eq("happy Oslo [a, b] [3, 1, 2] 10.1.2.3/24 [1,10) 1990-05-06", r.getMood().getLiteral() + " " + r.getHome().getCity()
          + " " + Arrays.toString(r.getTags()) + " " + Arrays.toString(r.getScores()) + " " + r.getIp() + " " + r.getLevelRange() + " " + r.getBorn());
      eq("{\"n\": [1, 2], \"theme\": \"dark\"}", r.getPrefs().data());
      eq("{\"k\":  1}", r.getRaw().data());
      eq("[2020-01-01,)", r.getMemberSince());
      ok(r.getCreatedAt() instanceof OffsetDateTime, "created_at");
    });
    check("rec::composite_pk", () -> {
      ctx.insertInto(PRODUCT, PRODUCT.SKU, PRODUCT.TITLE, PRODUCT.PRICE, PRODUCT.DIMS)
          .values("ABC-1", "pen", new BigDecimal("1.50"), new Integer[] {1, 2, 3}).execute();
      OrdersRecord o = ctx.newRecord(ORDERS); o.setCustomerId(1); o.setQty(2); o.store();
      OrderLineRecord l = ctx.newRecord(ORDER_LINE); l.setOrderId(o.getId()); l.setLineNo((short) 1); l.setProductId(1); l.setUnitPrice(new BigDecimal("1.40"));
      eq(1, l.store()); l.setQty(5); eq(1, l.store());
      eq(5, one(ctx, ORDER_LINE.QTY, ORDER_LINE.ORDER_ID.eq(o.getId()).and(ORDER_LINE.LINE_NO.eq((short) 1))));
      eq(1000, o.getInvoice());
    });
    check("rec::pojo", () -> {
      List<gen.shop.tables.pojos.Customer> all = ctx.selectFrom(CUSTOMER).orderBy(CUSTOMER.ID).fetchInto(gen.shop.tables.pojos.Customer.class);
      eq("[ann lee, bobby]", all.stream().map(gen.shop.tables.pojos.Customer::name).toList());
    });
    check("enum::filter_order", () -> {
      ctx.insertInto(CUSTOMER, CUSTOMER.NAME, CUSTOMER.MOOD).values("cat", mood("ok")).execute();
      eq("[bobby, cat, ann lee]", ctx.select(CUSTOMER.NAME).from(CUSTOMER).orderBy(CUSTOMER.MOOD).fetch(CUSTOMER.NAME));
      eq("[cat]", ctx.select(CUSTOMER.NAME).from(CUSTOMER).where(CUSTOMER.MOOD.eq(mood("ok"))).fetch(CUSTOMER.NAME));
      eq("[sad, ok, happy]", Arrays.stream(Mood.values()).map(Mood::getLiteral).toList());
    });
    check("udt::composite", () -> {
      ctx.update(CUSTOMER).set(CUSTOMER.HOME, new AddressRecord("2 Side Rd", "Bergen", null)).where(CUSTOMER.ID.eq(2)).execute();
      AddressRecord a = one(ctx, CUSTOMER.HOME, CUSTOMER.ID.eq(2));
      eq("2 Side Rd|Bergen|null", a.getStreet() + "|" + a.getCity() + "|" + a.getZip());
      eq("Bergen", ctx.fetchValue(select(field("({0}).city", String.class, CUSTOMER.HOME)).from(CUSTOMER).where(CUSTOMER.ID.eq(2))));
    });
    check("array::ops", () -> {
      ctx.update(CUSTOMER).set(CUSTOMER.TAGS, arrayAppend(CUSTOMER.TAGS, val("c"))).where(CUSTOMER.ID.eq(1)).execute();
      eq("[a, b, c]", Arrays.toString(one(ctx, CUSTOMER.TAGS, CUSTOMER.ID.eq(1))));
      eq(1, ctx.fetchCount(CUSTOMER, CUSTOMER.TAGS.contains(new String[] {"c", "a"})));
      eq(1, ctx.fetchCount(CUSTOMER, val("b").eq(any(CUSTOMER.TAGS))));
      eq("[1, 2, 3]", ctx.select(field(name("x"), Integer.class)).from(CUSTOMER, unnest(CUSTOMER.SCORES).as("t", "x")).where(CUSTOMER.ID.eq(1))
          .orderBy(field(name("x"))).fetch(0, Integer.class));
      eq(3, one(ctx, CUSTOMER, cardinality(CUSTOMER.SCORES), CUSTOMER.ID.eq(1)));
    });
    check("range::ops", () -> {
      eq(1, ctx.fetchCount(CUSTOMER, condition("{0} @> {1}", CUSTOMER.LEVEL_RANGE, val(5))));
      ctx.update(CUSTOMER).set(CUSTOMER.ACTIVE_DURING, OffsetDateTimeRange.offsetDateTimeRange(
          OffsetDateTime.of(2024, 1, 1, 0, 0, 0, 0, ZoneOffset.UTC), OffsetDateTime.of(2024, 6, 1, 0, 0, 0, 0, ZoneOffset.UTC)))
          .where(CUSTOMER.ID.eq(1)).execute();
      var r = one(ctx, CUSTOMER.ACTIVE_DURING, CUSTOMER.ID.eq(1));
      eq("2024-01-01T00:00Z", r.lower().toInstant().atOffset(ZoneOffset.UTC));
      eq(1, ctx.fetchCount(CUSTOMER, condition("{0} && tstzrange('2024-05-01', '2024-07-01')", CUSTOMER.ACTIVE_DURING)));
      eq("[2,10)", one(ctx, CUSTOMER, field("{0} * int4range(2, 20)", CUSTOMER.LEVEL_RANGE.getDataType(), CUSTOMER.LEVEL_RANGE), CUSTOMER.ID.eq(1)));
    });
    check("inet::ops", () -> {
      eq(1, ctx.fetchCount(CUSTOMER, condition("{0} << inet '10.1.0.0/16'", CUSTOMER.IP)));
      eq("10.1.2.0/24", one(ctx, CUSTOMER, field("network({0})::text", String.class, CUSTOMER.IP), CUSTOMER.ID.eq(1)));
    });
  }
}
EOF
cat > app/src/main/java/t/Checks2.java <<'EOF'
package t;

import static gen.shop.Tables.*;
import static org.jooq.impl.DSL.*;
import static t.T.*;
import static t.Checks.one;

import gen.shop.Sequences;
import gen.shop.tables.records.ProductRecord;
import java.math.BigDecimal;
import java.util.*;
import org.jooq.*;

/** DSL queries: joins, CTEs, windows, MULTISET, JSON, upserts, MERGE, batches, sequences, views. */
final class Checks2 {
  static void run(DSLContext ctx) {
    check("batch::insert_records", () -> {
      List<ProductRecord> ps = new ArrayList<>();
      for (int i = 2; i <= 6; i++) {
        ProductRecord p = new ProductRecord(); p.setSku("ABC-" + i); p.setTitle("item " + i);
        p.setPrice(new BigDecimal(i + ".25")); p.setDiscontinued(i % 2 == 0); ps.add(p);
      }
      eq(5, Arrays.stream(ctx.batchInsert(ps).execute()).sum());
    });
    check("batch::bind", () -> {
      var b = ctx.batch(ctx.insertInto(ORDERS, ORDERS.CUSTOMER_ID, ORDERS.QTY, ORDERS.NOTE).values((Integer) null, null, null));
      for (int i = 0; i < 20; i++) b.bind(i % 2 + 1, i + 1, "n" + i);
      eq(20, Arrays.stream(b.execute()).sum());
      eq(21, ctx.fetchCount(ORDERS));
      eq(21, ctx.fetchCount(gen.audit.Tables.EVENT));
    });
    check("dsl::join_group", () -> {
      var r = ctx.select(CUSTOMER.NAME, count(ORDERS.ID), sum(ORDERS.QTY)).from(CUSTOMER).leftJoin(ORDERS).onKey()
          .groupBy(CUSTOMER.NAME).orderBy(CUSTOMER.NAME).fetch();
      eq("ann lee:11:102 bobby:10:110 cat:0:null", String.join(" ", r.map(x -> x.value1() + ":" + x.value2() + ":" + x.value3())));
    });
    check("dsl::cte_window", () -> {
      var t = name("t").fields("n").as(select(val(1)).unionAll(select(field(name("n"), Integer.class).plus(1)).from(name("t")).where(field(name("n"), Integer.class).lt(5))));
      eq("[1, 2, 3, 4, 5]", ctx.withRecursive(t).select(t.field("n")).from(t).fetch(0, Integer.class));
      var w = ctx.select(ORDERS.ID, rowNumber().over(partitionBy(ORDERS.CUSTOMER_ID).orderBy(ORDERS.QTY.desc())).as("rn"),
          sum(ORDERS.QTY).over(orderBy(ORDERS.ID).rowsBetweenUnboundedPreceding().andCurrentRow()).as("run"))
          .from(ORDERS).orderBy(ORDERS.ID).limit(3).fetch();
      eq("1:10:2 2:11:3 3:10:5", String.join(" ", w.map(x -> x.value1() + ":" + x.value2() + ":" + x.value3())));
    });
    check("dsl::multiset", () -> {
      var r = ctx.select(CUSTOMER.NAME, multiset(select(ORDERS.QTY).from(ORDERS).where(ORDERS.CUSTOMER_ID.eq(CUSTOMER.ID))
          .orderBy(ORDERS.QTY).limit(3)).as("o").convertFrom(x -> x.map(Record1::value1)))
          .from(CUSTOMER).orderBy(CUSTOMER.ID).fetch();
      eq("ann lee=[1, 2, 3] bobby=[2, 4, 6] cat=[]", String.join(" ", r.map(x -> x.value1() + "=" + x.value2())));
    });
    check("dsl::lateral_distinctOn_setops", () -> {
      var o = select(ORDERS.QTY).from(ORDERS).where(ORDERS.CUSTOMER_ID.eq(CUSTOMER.ID)).orderBy(ORDERS.QTY.desc()).limit(1).asTable("o");
      eq("[19, 20]", ctx.select(o.field(ORDERS.QTY)).from(CUSTOMER, lateral(o)).orderBy(1).fetch(0, Integer.class));
      eq("[1, 2]", ctx.selectDistinct(ORDERS.CUSTOMER_ID).on(ORDERS.CUSTOMER_ID).from(ORDERS).orderBy(ORDERS.CUSTOMER_ID, ORDERS.QTY).fetch(0, Integer.class));
      eq("[1, 2, 4]", ctx.select(CUSTOMER.ID).from(CUSTOMER).except(select(val(9))).union(select(val(1))).orderBy(1).fetch(0, Integer.class));
      eq(2, ctx.fetchCount(selectFrom(CUSTOMER).whereExists(selectOne().from(ORDERS).where(ORDERS.CUSTOMER_ID.eq(CUSTOMER.ID)))));
    });
    check("dsl::grouping_sets", () -> {
      var r = ctx.select(PRODUCT.DISCONTINUED, count()).from(PRODUCT).groupBy(rollup(PRODUCT.DISCONTINUED))
          .orderBy(PRODUCT.DISCONTINUED.nullsLast()).fetch();
      eq("false:3 true:3 null:6", String.join(" ", r.map(x -> x.value1() + ":" + x.value2())));
    });
    check("json::functions", () -> {
      eq("{\"id\": 1, \"tags\": [\"a\", \"b\", \"c\"]}", ctx.select(jsonbObject(key("id").value(CUSTOMER.ID), key("tags").value(CUSTOMER.TAGS)))
          .from(CUSTOMER).where(CUSTOMER.ID.eq(1)).fetchOne(0, String.class));
      eq("\"dark\"", ctx.select(jsonbValue(CUSTOMER.PREFS, "$.theme")).from(CUSTOMER).where(CUSTOMER.ID.eq(1)).fetchOne(0, String.class));
      eq("[\"ABC-1\", \"ABC-2\", \"ABC-3\"]", ctx.select(jsonbArrayAgg(PRODUCT.SKU).orderBy(PRODUCT.SKU)).from(PRODUCT).where(PRODUCT.ID.le(3)).fetchOne(0, String.class));
      eq(1, ctx.fetchCount(CUSTOMER, condition("{0} @> {1}", CUSTOMER.PREFS, val(JSONB.valueOf("{\"theme\":\"dark\"}")))));
      eq("[1,2]", ctx.select(jsonbValue(CUSTOMER.PREFS, "$.n")).from(CUSTOMER).where(CUSTOMER.ID.eq(1)).fetchOne(0, String.class).replace(" ", ""));
    });
    check("upsert::on_conflict", () -> {
      eq(new BigDecimal("9.99"), ctx.insertInto(PRODUCT, PRODUCT.SKU, PRODUCT.PRICE).values("ABC-1", new BigDecimal("9.99"))
          .onConflict(PRODUCT.SKU).doUpdate().set(PRODUCT.PRICE, excluded(PRODUCT.PRICE)).returning(PRODUCT.PRICE).fetchOne().getPrice());
      eq(0, ctx.insertInto(PRODUCT, PRODUCT.SKU).values("ABC-2").onConflictDoNothing().execute());
      eq(1, ctx.insertInto(PRODUCT, PRODUCT.SKU, PRODUCT.TITLE).values("ABC-7", "new").onConflict(PRODUCT.SKU).doUpdate()
          .set(PRODUCT.TITLE, "upd").where(PRODUCT.DISCONTINUED.isFalse()).execute());
    });
    check("merge::native", () -> {
      var src = select(val("ABC-2").as("sku"), val(new BigDecimal("3.00")).as("price")).unionAll(select(val("XYZ-9"), val(new BigDecimal("4.00")))).asTable("s");
      int n = ctx.mergeInto(PRODUCT).using(src).on(PRODUCT.SKU.eq(src.field("sku", String.class)))
          .whenMatchedThenUpdate().set(PRODUCT.PRICE, src.field("price", BigDecimal.class))
          .whenNotMatchedThenInsert(PRODUCT.SKU, PRODUCT.PRICE).values(src.field("sku", String.class), src.field("price", BigDecimal.class)).execute();
      eq(2, n);
      eq("3.00 4.00", one(ctx, PRODUCT.PRICE, PRODUCT.SKU.eq("ABC-2")) + " " + one(ctx, PRODUCT.PRICE, PRODUCT.SKU.eq("XYZ-9")));
    });
    check("seq::nextval", () -> {
      long a = ctx.nextval(Sequences.INVOICE_SEQ); long b = ctx.nextval(Sequences.INVOICE_SEQ);
      eq(10, b - a); eq(b, ctx.currval(Sequences.INVOICE_SEQ));
    });
    check("view::summary_matview", () -> {
      eq("ann lee:11:102", ctx.selectFrom(CUSTOMER_SUMMARY).where(CUSTOMER_SUMMARY.ID.eq(1)).fetchOne(r -> r.getName() + ":" + r.getOrders() + ":" + r.getItems()));
      ctx.execute("refresh materialized view shop.product_stats");
      eq(2, ctx.fetchCount(PRODUCT_STATS));
    });
    check("dsl::static_statements", () -> {
      DSLContext s = using(ctx.configuration().derive(new org.jooq.conf.Settings().withStatementType(org.jooq.conf.StatementType.STATIC_STATEMENT)));
      eq(1, s.fetchCount(CUSTOMER, CUSTOMER.TAGS.contains(new String[] {"a"}).and(CUSTOMER.NAME.like("ann%")).and(CUSTOMER.PREFS.isNotNull())));
      eq("x'y", s.fetchValue(val("x'y")));
      eq("[1, 2]", Arrays.toString(s.fetchValue(val(new Integer[] {1, 2}))));
    });
  }
}
EOF
cat > app/src/main/java/t/Checks3.java <<'EOF'
package t;

import static gen.shop.Tables.*;
import static org.jooq.impl.DSL.*;
import static t.T.*;
import static t.Checks.one;

import gen.shop.Routines;
import gen.shop.routines.*;
import gen.shop.udt.records.AddressRecord;
import java.math.BigDecimal;
import java.time.LocalDate;
import java.util.*;
import org.jooq.*;

/** Routines, transactions, cursors, partitions, inheritance, runtime Meta. */
final class Checks3 {
  static void run(DSLContext ctx) {
    Configuration cfg = ctx.configuration();
    check("routine::scalar_out", () -> {
      eq(11, Routines.orderCount(cfg, 1));
      eq(11, one(ctx, CUSTOMER, Routines.orderCount(CUSTOMER.ID), CUSTOMER.ID.eq(1)));
      SplitName s = new SplitName(); s.setFullName("ann lee"); s.execute(cfg);
      eq("ann/lee", s.getFirst() + "/" + s.getLast());
    });
    check("routine::array_udt_overload", () -> {
      eq("[2, 3, 4]", Arrays.toString(Routines.bumpAll(cfg, new Integer[] {1, 2, 3}, 1)));
      eq("1 Main St, Oslo", Routines.fmt(cfg, new AddressRecord("1 Main St", "Oslo", null)));
      eq("8 abab", ctx.fetchValue(Routines.twice1(val(4))) + " " + ctx.fetchValue(Routines.twice2(val("ab"))));
    });
    check("routine::table_valued", () -> eq("[cat]",
        ctx.select(CUSTOMERS_WITH.NAME).from(CUSTOMERS_WITH.call(Checks.mood("ok"))).fetch(CUSTOMERS_WITH.NAME)));
    check("routine::procedure_inout", () -> {
      RenameCustomer p = new RenameCustomer(); p.setCid(4); p.setNewName("kat"); p.setOldName(null); p.execute(cfg);
      eq("cat", p.getOldName()); eq("kat", one(ctx, CUSTOMER.NAME, CUSTOMER.ID.eq(4)));
    });
    check("routine::aggregate", () -> eq("25.0000",
        ctx.select(Routines.sumSq(PRODUCT.PRICE)).from(PRODUCT).where(PRODUCT.SKU.in("ABC-2", "XYZ-9")).fetchOne(0, BigDecimal.class)));
    check("tx::rollback_savepoint", () -> {
      int before = ctx.fetchCount(ORDERS);
      try {
        ctx.transaction(c -> { using(c).insertInto(ORDERS, ORDERS.CUSTOMER_ID, ORDERS.QTY).values(1, 1).execute(); throw new RuntimeException("boom"); });
      } catch (RuntimeException expected) { }
      eq(before, ctx.fetchCount(ORDERS));
      ctx.transaction(c -> {
        using(c).insertInto(ORDERS, ORDERS.CUSTOMER_ID, ORDERS.QTY).values(1, 1).execute();
        try {
          using(c).transaction(n -> { using(n).insertInto(ORDERS, ORDERS.CUSTOMER_ID, ORDERS.QTY).values(1, -1).execute(); });
        } catch (org.jooq.exception.DataAccessException expected) { }
      });
      eq(before + 1, ctx.fetchCount(ORDERS));
    });
    check("tx::lazy_cursor_skipLocked", () -> {
      ctx.transaction(c -> {
        long sum = 0;
        try (Cursor<Record1<Integer>> cur = using(c).select(field("g", Integer.class)).from(generateSeries(1, 500).as("s", "g")).fetchSize(17).fetchLazy()) {
          for (Record1<Integer> r : cur) sum += r.value1();
        }
        eq(125250, sum);
        eq(22, using(c).selectFrom(ORDERS).forUpdate().skipLocked().fetch().size());
      });
    });
    check("dsl::partition_inheritance", () -> {
      ctx.insertInto(MEASUREMENT, MEASUREMENT.ID, MEASUREMENT.TAKEN, MEASUREMENT.V).values(1L, LocalDate.of(2025, 3, 1), 1.5)
          .values(2L, LocalDate.of(2027, 1, 1), 2.5).execute();
      eq("measurement_2025 measurement_rest", one(ctx, MEASUREMENT, field("string_agg(tableoid::regclass::text, ' ' order by id)", String.class), MEASUREMENT.ID.gt(0L)).replace("shop.", ""));
      ctx.insertInto(SPECIAL_DOC, SPECIAL_DOC.BODY, SPECIAL_DOC.STAMP).values("child", "s").execute();
      ctx.insertInto(BASE_DOC, BASE_DOC.BODY).values("parent").execute();
      eq("2 1", ctx.fetchCount(BASE_DOC) + " " + ctx.fetchValue("select count(*) from only shop.base_doc"));
    });
    check("dsl::plain_sql", () -> {
      eq("ann lee", ctx.fetchValue("select name from shop.customer where id = ?", 1));
      eq(3, ctx.fetch("select * from shop.customer order by id").size());
      eq("[a, b, c]", Arrays.toString((Object[]) ctx.fetchValue("select tags from shop.customer where id = {0}", val(1))));
    });
    check("meta::jdbc_tables", () -> {
      Meta m = ctx.meta();
      List<String> schemas = m.getSchemas().stream().map(Schema::getName).filter(n -> n.equals("shop") || n.equals("audit")).sorted().toList();
      eq("[audit, shop]", schemas);
      Schema shop = m.getSchemas("shop").get(0);
      List<String> tables = shop.getTables().stream().map(t -> t.getName() + ":" + t.getType()).sorted().toList();
      T.save("jooq-meta-tables.txt", String.join("\n", tables));
      blob("meta::tables", String.join("\n", tables));
      Table<?> c = shop.getTable("customer");
      eq("id", c.getPrimaryKey().getFields().get(0).getName());
      eq("[orders_customer_id_fkey]", shop.getTable("orders").getReferences().stream().map(Named::getName).toList());
      for (Table<?> t : shop.getTables())
        blob("meta::table::" + t.getName(), Arrays.stream(t.fields()).map(f -> f.getName() + " " + f.getDataType().getTypeName()
            + (f.getDataType().nullable() ? "" : " not null") + (f.getDataType().defaulted() ? " default" : "")).reduce("", (a, b) -> a + b + "\n")
            + t.getIndexes().stream().map(i -> i.getName() + (i.getUnique() ? " unique" : "")).sorted().toList()
            + t.getKeys().stream().map(k -> k.getName()).sorted().toList());
    });
    check("meta::ddl", () -> {
      Queries q = ctx.meta().filterSchemas(s -> s.getName().equals("shop")).ddl();
      String all = q.toString();
      T.save("jooq-meta-ddl.sql", all);
      for (Query x : q.queries()) {
        String sql = x.getSQL();
        String head = sql.replaceAll("(?s)^(\\w+ \\w+ [\\w.\"]+).*", "$1");
        blob("ddl::" + head, sql);
      }
      ok(q.queries().length > 10, "only " + q.queries().length + " DDL statements");
    });
  }
}
EOF
cd $W/gen
$MVN package dependency:copy-dependencies -DincludeScope=runtime > "$OUT/build-gen.log" 2>&1 || { tail -30 "$OUT/build-gen.log"; exit 1; }
timeout 600 java -cp "target/classes:target/dependency/*" t.Gen $W/gen/schema.sql $W/app/src/main/java > "$OUT/codegen.log" 2>&1 \
  || { tail -30 "$OUT/codegen.log"; echo "FAIL jooq::codegen::run" > "$OUT/jooq.lst"; exit 1; }
tail -1 "$OUT/codegen.log"
mkdir -p "$OUT/generated" && cp -r $W/app/src/main/java/gen "$OUT/generated/"
cd $W/app
$MVN package dependency:copy-dependencies -DincludeScope=runtime > "$OUT/build-app.log" 2>&1 \
  || { grep -E 'ERROR.*java' "$OUT/build-app.log" | head -30; echo "FAIL jooq::app::compile" > "$OUT/jooq.lst"; exit 1; }
timeout 900 java -cp "target/classes:target/dependency/*" t.Main $W/app/src/main/java/gen > "$OUT/jooq.log" 2>&1 || echo "jooq exited $?"
grep -E '^(FAIL |[a-z]+:: pass)' "$OUT/jooq.log" | cut -c1-400
