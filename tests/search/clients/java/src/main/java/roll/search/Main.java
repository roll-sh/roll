// elasticsearch-java 9.x against roll search: the shared client scenario through the typed
// client and its BulkIngester helper. The Rest5Client transport checks the product header on
// every response.
//
// Usage: java roll.search.Main   (ES_URL, default http://127.0.0.1:59200)
package roll.search;

import co.elastic.clients.elasticsearch.ElasticsearchClient;
import co.elastic.clients.elasticsearch._helpers.bulk.BulkIngester;
import co.elastic.clients.elasticsearch._types.ElasticsearchException;
import co.elastic.clients.elasticsearch._types.FieldValue;
import co.elastic.clients.elasticsearch._types.Refresh;
import co.elastic.clients.elasticsearch._types.SortOrder;
import co.elastic.clients.elasticsearch._types.aggregations.Aggregation;
import co.elastic.clients.elasticsearch._types.aggregations.CalendarInterval;
import co.elastic.clients.elasticsearch._types.aggregations.StringTermsBucket;
import co.elastic.clients.elasticsearch.core.SearchResponse;
import co.elastic.clients.elasticsearch.core.search.Hit;
import co.elastic.clients.json.JsonData;
import co.elastic.clients.json.jackson.JacksonJsonpMapper;
import co.elastic.clients.transport.rest5_client.Rest5ClientTransport;
import co.elastic.clients.transport.rest5_client.low_level.Rest5Client;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.TreeMap;
import java.util.concurrent.TimeUnit;

public class Main {
    static final String IDX = "clients-java";
    static final String[] COLORS = {"red", "green", "blue"};
    static final List<String> failures = new ArrayList<>();

    interface Check {
        void run() throws Exception;
    }

    static void check(String name, Check c) {
        try {
            c.run();
            System.out.println("ok   " + name);
        } catch (Throwable e) {
            failures.add(name);
            System.out.println("FAIL " + name + ": " + e);
            e.printStackTrace(System.out);
        }
    }

    static void eq(Object got, Object want, String what) {
        if (!Objects.equals(got, want)) {
            throw new AssertionError(what + ": expected " + want + ", got " + got);
        }
    }

    static Map<String, Object> doc(int i) {
        Map<String, Object> d = new HashMap<>();
        d.put("title", "document " + i + " " + (i % 2 == 1 ? "quick brown fox" : "lazy dog"));
        d.put("tag", COLORS[i % 3]);
        d.put("n", i);
        d.put("price", i * 1.5);
        d.put("d", String.format("2024-%02d-%02d", i % 12 + 1, i % 28 + 1));
        d.put("comments", List.of(Map.of("author", "a" + (i % 5), "stars", i % 5)));
        return d;
    }

    @SuppressWarnings({"rawtypes", "unchecked"})
    public static void main(String[] args) throws Exception {
        String url = System.getenv().getOrDefault("ES_URL", "http://127.0.0.1:59200");
        Rest5Client rest = Rest5Client.builder(java.net.URI.create(url)).build();
        var transport = new Rest5ClientTransport(rest, new JacksonJsonpMapper());
        ElasticsearchClient es = new ElasticsearchClient(transport);

        check("info and product check", () -> {
            var info = es.info();
            eq(info.version().number(), "9.5.5", "version");
            eq(info.tagline(), "You Know, for Search", "tagline");
            eq(es.ping().value(), true, "ping");
        });

        check("create index with mappings", () -> {
            try {
                es.indices().delete(d -> d.index(IDX + "*"));
            } catch (ElasticsearchException ignored) {
            }
            var r = es.indices().create(c -> c.index(IDX)
                .settings(s -> s.numberOfShards("2").numberOfReplicas("0"))
                .mappings(m -> m
                    .properties("title", p -> p.text(t -> t.fields("raw", f -> f.keyword(k -> k))))
                    .properties("tag", p -> p.keyword(k -> k))
                    .properties("n", p -> p.integer(x -> x))
                    .properties("price", p -> p.double_(x -> x))
                    .properties("d", p -> p.date(x -> x))
                    .properties("comments", p -> p.nested(n -> n.properties("author", q -> q.keyword(k -> k)).properties("stars", q -> q.integer(x -> x))))));
            eq(r.acknowledged(), true, "acknowledged");
            eq(es.indices().exists(e -> e.index(IDX)).value(), true, "exists");
        });

        check("BulkIngester helper", () -> {
            try (BulkIngester<Void> ing = BulkIngester.of(b -> b.client(es).maxOperations(17).flushInterval(1, TimeUnit.SECONDS))) {
                for (int i = 0; i < 100; i++) {
                    final int n = i;
                    ing.add(op -> op.index(x -> x.index(IDX).id(String.valueOf(n)).document(doc(n))));
                }
            }
            es.indices().refresh(r -> r.index(IDX));
            eq(es.count(c -> c.index(IDX)).count(), 100L, "count");
        });

        check("search with query and aggregations", () -> {
            SearchResponse<Map> r = es.search(s -> s.index(IDX)
                .query(q -> q.bool(b -> b
                    .must(m -> m.match(mm -> mm.field("title").query("fox")))
                    .filter(f -> f.range(rg -> rg.number(nr -> nr.field("n").gte(10.0).lt(60.0))))))
                .aggregations("tags", a -> a.terms(t -> t.field("tag")).aggregations("avg_n", sub -> sub.avg(v -> v.field("n"))))
                .aggregations("months", a -> a.dateHistogram(h -> h.field("d").calendarInterval(CalendarInterval.Month)))
                .aggregations("price", a -> a.stats(v -> v.field("price")))
                .aggregations("stars", a -> a.nested(n -> n.path("comments")).aggregations("max", sub -> sub.max(v -> v.field("comments.stars"))))
                .size(5)
                .sort(so -> so.field(f -> f.field("n").order(SortOrder.Asc)))
                .highlight(h -> h.fields(co.elastic.clients.util.NamedValue.of("title", co.elastic.clients.elasticsearch.core.search.HighlightField.of(f -> f)))), Map.class);
            eq(r.hits().total().value(), 25L, "total");
            List<String> ids = new ArrayList<>();
            for (Hit<Map> h : r.hits().hits()) ids.add(h.id());
            eq(ids, List.of("11", "13", "15", "17", "19"), "ids");
            eq(r.hits().hits().get(0).highlight().get("title"), List.of("document 11 quick brown <em>fox</em>"), "highlight");
            Map<String, String> buckets = new TreeMap<>();
            for (StringTermsBucket b : r.aggregations().get("tags").sterms().buckets().array()) {
                buckets.put(b.key().stringValue(), b.docCount() + "/" + b.aggregations().get("avg_n").avg().value());
            }
            eq(buckets.toString(), "{blue=9/35.0, green=8/34.0, red=8/36.0}", "terms");
            eq(r.aggregations().get("price").stats().count(), 25L, "stats.count");
            eq(r.aggregations().get("stars").nested().aggregations().get("max").max().value(), 4.0, "nested max");
            long total = 0;
            for (var b : r.aggregations().get("months").dateHistogram().buckets().array()) total += b.docCount();
            eq(total, 25L, "date_histogram");
        });

        check("documents: get, update, conflicts, delete", () -> {
            var g = es.get(x -> x.index(IDX).id("5"), Map.class);
            eq(((Number) g.source().get("n")).intValue(), 5, "get");
            eq(es.exists(x -> x.index(IDX).id("nope")).value(), false, "exists");
            var u = es.update(x -> x.index(IDX).id("5").doc(Map.of("n", 500)).refresh(Refresh.True), Map.class);
            eq(u.result().jsonValue(), "updated", "update");
            try {
                es.index(x -> x.index(IDX).id("5").document(Map.of("n", 1)).ifSeqNo(0L).ifPrimaryTerm(1L));
                throw new AssertionError("expected a conflict");
            } catch (ElasticsearchException e) {
                eq(e.status(), 409, "status");
                eq(e.error().type(), "version_conflict_engine_exception", "type");
            }
            es.index(x -> x.index(IDX).id("tmp").document(Map.of("n", -1)).refresh(Refresh.WaitFor));
            eq(es.delete(x -> x.index(IDX).id("tmp").refresh(Refresh.True)).result().jsonValue(), "deleted", "delete");
        });

        check("scroll", () -> {
            SearchResponse<Map> r = es.search(s -> s.index(IDX).scroll(t -> t.time("1m")).size(7), Map.class);
            int seen = r.hits().hits().size();
            String sid = r.scrollId();
            while (true) {
                final String id = sid;
                var s = es.scroll(x -> x.scrollId(id).scroll(t -> t.time("1m")), Map.class);
                if (s.hits().hits().isEmpty()) break;
                seen += s.hits().hits().size();
                sid = s.scrollId();
            }
            eq(seen, 100, "scrolled");
            final String last = sid;
            eq(es.clearScroll(c -> c.scrollId(last)).succeeded(), true, "clear");
        });

        check("point in time with search_after", () -> {
            String pit = es.openPointInTime(p -> p.index(IDX).keepAlive(t -> t.time("1m"))).id();
            HashSet<String> seen = new HashSet<>();
            List<FieldValue> after = null;
            while (true) {
                final String id = pit;
                final List<FieldValue> a = after;
                SearchResponse<Map> r = es.search(s -> {
                    s.pit(p -> p.id(id).keepAlive(t -> t.time("1m"))).size(30).sort(so -> so.field(f -> f.field("n").order(SortOrder.Asc)));
                    if (a != null) s.searchAfter(a);
                    return s;
                }, Map.class);
                if (r.hits().hits().isEmpty()) break;
                for (Hit<Map> h : r.hits().hits()) seen.add(h.id());
                after = r.hits().hits().get(r.hits().hits().size() - 1).sort();
                pit = r.pitId();
            }
            eq(seen.size(), 100, "paged");
            final String id = pit;
            eq(es.closePointInTime(c -> c.id(id)).succeeded(), true, "close");
        });

        check("update_by_query and delete_by_query", () -> {
            var u = es.updateByQuery(x -> x.index(IDX).query(q -> q.term(t -> t.field("tag").value("blue")))
                .script(sc -> sc.source(src -> src.scriptString("ctx._source.n += params.k")).params("k", JsonData.of(1000))).refresh(true));
            eq(u.updated(), 33L, "updated");
            var d = es.deleteByQuery(x -> x.index(IDX).query(q -> q.term(t -> t.field("tag").value("green"))).refresh(true));
            eq(d.deleted(), 33L, "deleted");
            eq(es.count(c -> c.index(IDX)).count(), 67L, "count");
        });

        check("errors", () -> {
            try {
                es.search(s -> s.index("clients-java-missing"), Map.class);
                throw new AssertionError("expected 404");
            } catch (ElasticsearchException e) {
                eq(e.status(), 404, "status");
                eq(e.error().type(), "index_not_found_exception", "type");
                eq(e.error().reason(), "no such index [clients-java-missing]", "reason");
            }
        });

        check("cat and cluster", () -> {
            eq(es.cluster().health(h -> h.index(IDX)).status().jsonValue(), "green", "health");
            var rows = es.cat().indices(c -> c.index(IDX + "*")).indices();
            eq(rows.size(), 1, "rows");
            eq(rows.get(0).index(), IDX, "index");
        });

        check("delete index", () -> eq(es.indices().delete(d -> d.index(IDX + "*")).acknowledged(), true, "acknowledged"));

        transport.close();
        if (!failures.isEmpty()) {
            System.out.println("elasticsearch-java: " + failures.size() + " check(s) failed: " + String.join(", ", failures));
            System.exit(1);
        }
        System.out.println("elasticsearch-java: all checks passed");
    }
}
