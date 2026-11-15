// Lucene side of `tests/bench.rs` `bench_vs_lucene`: indexes the exported corpus (one document
// per line, whitespace-separated tokens) into one merged segment with Lucene's defaults (BM25,
// positions and norms) and times the exported query sets with top-10 and a total-hits threshold
// of 1000, single-threaded, best of BENCH_ROUNDS (5) rounds after JIT warm-up. Run with `run.sh`.

import java.nio.file.*;
import java.util.*;
import org.apache.lucene.analysis.Analyzer;
import org.apache.lucene.analysis.core.WhitespaceTokenizer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.search.*;
import org.apache.lucene.store.*;

public class LuceneBench {
    public static void main(String[] args) throws Exception {
        Path dir = Paths.get(args[0]);
        Analyzer an = new Analyzer() {
            @Override
            protected TokenStreamComponents createComponents(String f) {
                return new TokenStreamComponents(new WhitespaceTokenizer());
            }
        };
        Path idx = dir.resolve("lucene-index");
        Directory d = FSDirectory.open(idx);
        if (!DirectoryReader.indexExists(d)) {
            IndexWriterConfig cfg = new IndexWriterConfig(an).setRAMBufferSizeMB(256);
            long t0 = System.nanoTime();
            try (IndexWriter w = new IndexWriter(d, cfg)) {
                List<String> nums = Files.exists(dir.resolve("nums.txt")) ? Files.readAllLines(dir.resolve("nums.txt")) : List.of();
                int n = 0;
                for (String line : Files.readAllLines(dir.resolve("corpus.txt"))) {
                    Document doc = new Document();
                    doc.add(new TextField("body", line, Field.Store.NO));
                    if (n < nums.size()) {
                        long v = Long.parseLong(nums.get(n));
                        doc.add(new LongPoint("num", v));
                        doc.add(new SortedNumericDocValuesField("num", v));
                    }
                    w.addDocument(doc);
                    n++;
                }
                w.commit();
                long t1 = System.nanoTime();
                System.out.printf("lucene: indexed %d docs in %.2fs (%.0f docs/s)%n", n, (t1 - t0) / 1e9, n / ((t1 - t0) / 1e9));
                w.forceMerge(1);
            }
        }
        DirectoryReader r = DirectoryReader.open(d);
        IndexSearcher s = new IndexSearcher(r);
        LinkedHashMap<String, List<Query>> sets = new LinkedHashMap<>();
        for (String line : Files.readAllLines(dir.resolve("queries.txt"))) {
            String[] p = line.split("\t");
            String[] terms = p.length > 2 ? p[2].split(" ") : new String[0];
            Query q;
            switch (p[1]) {
                case "sortall" -> q = new MatchAllDocsQuery();
                case "sortterm" -> q = new TermQuery(new Term("body", terms[0]));
                case "term" -> q = new TermQuery(new Term("body", terms[0]));
                case "and", "or" -> {
                    BooleanQuery.Builder b = new BooleanQuery.Builder();
                    for (String t : terms) {
                        b.add(new TermQuery(new Term("body", t)), p[1].equals("and") ? BooleanClause.Occur.MUST : BooleanClause.Occur.SHOULD);
                    }
                    q = b.build();
                }
                case "phrase" -> q = new PhraseQuery(0, "body", terms);
                default -> q = new PhraseQuery(2, "body", terms);
            }
            sets.computeIfAbsent(p[0], k -> new ArrayList<>()).add(q);
        }
        System.out.printf("lucene %s: %d docs, %d segment(s)%n", org.apache.lucene.util.Version.LATEST, r.numDocs(), r.leaves().size());
        // JIT warm-up over every set.
        for (int i = 0; i < 20; i++) {
            for (Map.Entry<String, List<Query>> e : sets.entrySet()) {
                for (Query q : e.getValue()) {
                    if (e.getKey().startsWith("sort-")) {
                        s.search(q, new TopFieldCollectorManager(new Sort(new SortedNumericSortField("num", SortField.Type.LONG, true)), 10, 1000));
                    } else {
                        s.search(q, new TopScoreDocCollectorManager(10, 1000));
                    }
                }
            }
        }
        // BENCH_ROUNDS (default 5) and BENCH_ONLY=<set>, as on the roll side.
        int rounds = Integer.parseInt(System.getenv().getOrDefault("BENCH_ROUNDS", "5"));
        String only = System.getenv("BENCH_ONLY");
        for (Map.Entry<String, List<Query>> e : sets.entrySet()) {
            if (only != null && !only.equals(e.getKey())) {
                continue;
            }
            double best = Double.MAX_VALUE;
            long hits = 0;
            for (int round = 0; round < rounds; round++) {
                hits = 0;
                long t = System.nanoTime();
                for (Query q : e.getValue()) {
                    TopDocs td = e.getKey().startsWith("sort-")
                        ? s.search(q, new TopFieldCollectorManager(new Sort(new SortedNumericSortField("num", SortField.Type.LONG, true)), 10, 1000))
                        : s.search(q, new TopScoreDocCollectorManager(10, 1000));
                    hits += td.totalHits.value();
                }
                best = Math.min(best, (System.nanoTime() - t) / 1e3 / e.getValue().size());
            }
            System.out.printf("%-14s %9.1f µs/query  (avg total %d)%n", e.getKey(), best, hits / e.getValue().size());
        }
        r.close();
    }
}
