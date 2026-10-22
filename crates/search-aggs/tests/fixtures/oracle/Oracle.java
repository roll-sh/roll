import org.elasticsearch.tdigest.*;
import org.elasticsearch.tdigest.arrays.*;
import org.HdrHistogram.DoubleHistogram;
import java.util.*;

/**
 * Reference outputs for `tests/digest_oracle.rs`, from the digests Elasticsearch 9.5.5 runs.
 * Regenerate (jars copied from the reference node's `lib/`):
 * java -cp elasticsearch-tdigest-9.5.5.jar:elasticsearch-core-9.5.5.jar:lucene-core-10.5.1.jar:HdrHistogram-2.1.9.jar \
 *     Oracle.java > ../aggs-digest-oracle.json
 */
public class Oracle {
    static final class DArr implements TDigestDoubleArray {
        double[] a; int size;
        DArr(int n) { a = new double[n]; size = n; }
        public int size() { return size; }
        public double get(int i) { return a[i]; }
        public void set(int i, double v) { a[i] = v; }
        public void add(double v) { ensureCapacity(size + 1); a[size++] = v; }
        public void ensureCapacity(int n) { if (n > a.length) a = Arrays.copyOf(a, Math.max(n, a.length * 2 + 1)); }
        public void resize(int n) { ensureCapacity(n); if (n > size) Arrays.fill(a, size, n, 0); size = n; }
        public void sort() { Arrays.sort(a, 0, size); }
        public long ramBytesUsed() { return 0; }
        public void close() {}
    }
    static final class IArr implements TDigestIntArray {
        int[] a; int size;
        IArr(int n) { a = new int[n]; size = n; }
        public int size() { return size; }
        public int get(int i) { return a[i]; }
        public void set(int i, int v) { a[i] = v; }
        public void resize(int n) { if (n > a.length) a = Arrays.copyOf(a, n); if (n > size) Arrays.fill(a, size, n, 0); size = n; }
        public long ramBytesUsed() { return 0; }
        public void close() {}
    }
    static final class LArr implements TDigestLongArray {
        long[] a; int size;
        LArr(int n) { a = new long[n]; size = n; }
        public int size() { return size; }
        public long get(int i) { return a[i]; }
        public void set(int i, long v) { a[i] = v; }
        public void resize(int n) { if (n > a.length) a = Arrays.copyOf(a, n); if (n > size) Arrays.fill(a, size, n, 0); size = n; }
        public long ramBytesUsed() { return 0; }
        public void close() {}
    }
    static final class BArr implements TDigestByteArray {
        byte[] a; int size;
        BArr(int n) { a = new byte[n]; size = n; }
        public int size() { return size; }
        public byte get(int i) { return a[i]; }
        public void set(int i, byte v) { a[i] = v; }
        public void resize(int n) { if (n > a.length) a = Arrays.copyOf(a, n); if (n > size) Arrays.fill(a, size, n, (byte) 0); size = n; }
        public long ramBytesUsed() { return 0; }
        public void close() {}
    }
    static final TDigestArrays ARR = new TDigestArrays() {
        public void adjustBreaker(long b) {}
        public TDigestDoubleArray newDoubleArray(int n) { return new DArr(n); }
        public TDigestIntArray newIntArray(int n) { return new IArr(n); }
        public TDigestLongArray newLongArray(int n) { return new LArr(n); }
        public TDigestByteArray newByteArray(int n) { return new BArr(n); }
    };

    static double value(Random r, int dist) {
        switch (dist) {
            case 0: return r.nextDouble() * 1000.0;
            case 1: return Math.floor(r.nextDouble() * 1000.0) / 10.0;
            case 2: { double u = r.nextDouble(); return u * u * u * 100000.0; }
            case 3: return r.nextInt(50);
            default: return r.nextDouble() * r.nextDouble() * 1e9 + 1e-3;
        }
    }

    static TDigest make(String kind, double c) {
        switch (kind) {
            case "hybrid": return TDigest.createHybridDigest(ARR, c);
            case "merging": return TDigest.createMergingDigest(ARR, c);
            default: { AVLTreeDigest d = TDigest.createAvlTreeDigest(ARR, c); d.setRandomSeed(0x2545F4914F6CDD1DL); return d; }
        }
    }

    static String hex(double d) { return "\"" + Long.toHexString(Double.doubleToRawLongBits(d)) + "\""; }
    static final double[] QS = {0, 0.001, 0.01, 0.05, 0.25, 0.5, 0.75, 0.95, 0.99, 0.999, 1};

    public static void main(String[] args) {
        StringBuilder out = new StringBuilder("{\"tdigest\":[\n");
        String[] kinds = {"hybrid", "avl"};
        double[] comps = {100, 7.5, 1000};
        int[] ns = {1, 2, 10, 1999, 2000, 2001, 12000};
        boolean first = true;
        long seed = 1;
        for (String kind : kinds) for (double c : comps) for (int n : ns) for (int dist = 0; dist < 5; dist++) for (int parts : new int[]{1, 3}) for (int sortFirst = 0; sortFirst < 2; sortFirst++) {
            if (kind.equals("avl") && (n > 5000 || c > 200)) continue;
            if (n < 1999 && dist != 0 && dist != 3) continue;
            if (parts == 3 && sortFirst == 1) continue;
            seed++;
            Random r = new Random(seed);
            TDigest[] ds = new TDigest[parts];
            for (int p = 0; p < parts; p++) ds[p] = make(kind, c);
            for (int i = 0; i < n; i++) ds[i % parts].add(value(r, dist));
            TDigest d;
            StringBuilder q = new StringBuilder();
            if (sortFirst == 1) {
                // metric() during terms ordering: quantile before compress().
                q.append(hex(ds[0].quantile(0.5)));
            }
            if (parts == 1) {
                d = ds[0];
                d.compress();
            } else {
                for (TDigest x : ds) x.compress();
                d = make(kind, c);
                for (TDigest x : ds) { d.reserve(x.size()); d.add(x); }
                d.compress();
            }
            StringBuilder qs = new StringBuilder();
            for (double qq : QS) { if (qs.length() > 0) qs.append(','); qs.append(hex(d.quantile(qq))); }
            StringBuilder cs = new StringBuilder();
            double lo = d.getMin(), hi = d.getMax();
            for (int k = 0; k <= 10; k++) { double x = lo + (hi - lo) * k / 10.0; if (cs.length() > 0) cs.append(','); cs.append(hex(d.cdf(x))); }
            if (!first) out.append(",\n");
            first = false;
            out.append("{\"kind\":\"").append(kind).append("\",\"c\":").append(c).append(",\"n\":").append(n).append(",\"dist\":").append(dist)
               .append(",\"parts\":").append(parts).append(",\"seed\":").append(seed).append(",\"sort_first\":").append(sortFirst == 1 ? "[" + q + "]" : "[]")
               .append(",\"centroids\":").append(d.centroidCount()).append(",\"q\":[").append(qs).append("],\"cdf\":[").append(cs).append("]}");
        }
        out.append("\n],\"hdr\":[\n");
        first = true;
        double[] ps = {0, 1, 5, 25, 50, 75, 95, 99, 99.9, 100};
        for (int nsd = 0; nsd <= 5; nsd++) for (int n : new int[]{1, 3, 100, 5000}) for (int dist = 0; dist < 5; dist++) for (int parts : new int[]{1, 3}) {
            seed++;
            Random r = new Random(seed);
            DoubleHistogram[] hs = new DoubleHistogram[parts];
            for (int p = 0; p < parts; p++) { hs[p] = new DoubleHistogram(nsd); hs[p].setAutoResize(true); }
            String err = null;
            try {
                for (int i = 0; i < n; i++) hs[i % parts].recordValue(value(r, dist));
            } catch (ArrayIndexOutOfBoundsException e) { err = e.getMessage(); }
            if (err != null) {
                if (!first) out.append(",\n");
                first = false;
                out.append("{\"nsd\":").append(nsd).append(",\"n\":").append(n).append(",\"dist\":").append(dist).append(",\"parts\":").append(parts)
                   .append(",\"seed\":").append(seed).append(",\"error\":\"").append(err.replace("\n", "\\n").replace("\"", "'")).append("\"}");
                continue;
            }
            DoubleHistogram h;
            if (parts == 1) h = hs[0];
            else {
                h = new DoubleHistogram(hs[0]);
                h.setAutoResize(true);
                for (DoubleHistogram x : hs) {
                    DoubleHistogram more = h, less = x;
                    if (x.getNumberOfSignificantValueDigits() > h.getNumberOfSignificantValueDigits()) { more = x; less = h; }
                    more.setAutoResize(true);
                    more.add(less);
                    h = more;
                }
            }
            StringBuilder qs = new StringBuilder();
            for (double p : ps) { if (qs.length() > 0) qs.append(','); qs.append(hex(h.getValueAtPercentile(p))); }
            StringBuilder cs = new StringBuilder();
            double lo = h.getMinValue(), hi = h.getMaxValue();
            for (int k = 0; k <= 10; k++) { double x = lo + (hi - lo) * k / 10.0; if (cs.length() > 0) cs.append(','); cs.append(hex(h.getPercentileAtOrBelowValue(x))); }
            if (!first) out.append(",\n");
            first = false;
            out.append("{\"nsd\":").append(nsd).append(",\"n\":").append(n).append(",\"dist\":").append(dist).append(",\"parts\":").append(parts)
               .append(",\"seed\":").append(seed).append(",\"total\":").append(h.getTotalCount()).append(",\"min\":").append(hex(lo)).append(",\"max\":").append(hex(hi))
               .append(",\"p\":[").append(qs).append("],\"rank\":[").append(cs).append("]}");
        }
        out.append("\n]}\n");
        System.out.print(out);
    }
}
