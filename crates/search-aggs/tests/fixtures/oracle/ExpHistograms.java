import org.elasticsearch.exponentialhistogram.*;
import org.elasticsearch.search.aggregations.metrics.HistogramUnionState;
import org.elasticsearch.tdigest.Centroid;

import java.util.*;

/**
 * Reference outputs for `tests/exp_histogram.rs`: Elasticsearch 9.5.5's own exponential histogram
 * library (`ExponentialScaleUtils`, `ExponentialHistogramMerger`, `ExponentialHistogramQuantile`)
 * and `HistogramUnionState` (digest + exponential histogram) on random inputs. Doubles are raw
 * bit patterns in hex. Compile and run against the reference node's `lib/` jars:
 * javac -cp 'lib/*' -d out ExpHistograms.java
 * java -cp 'out:lib/*' ExpHistograms > ../aggs-exp-histograms.json
 */
public class ExpHistograms {
    static final ExponentialHistogramCircuitBreaker NOOP = ExponentialHistogramCircuitBreaker.noop();
    static StringBuilder out = new StringBuilder("{\n");

    static String hex(double d) { return "\"" + Long.toHexString(Double.doubleToRawLongBits(d)) + "\""; }

    static String buckets(ExponentialHistogram.Buckets b) {
        StringBuilder s = new StringBuilder("[");
        BucketIterator it = b.iterator();
        boolean first = true;
        while (it.hasNext()) {
            s.append(first ? "" : ",").append("[").append(it.peekIndex()).append(",").append(it.peekCount()).append("]");
            first = false;
            it.advance();
        }
        return s.append("]").toString();
    }

    static String hist(ExponentialHistogram h) {
        ZeroBucket z = h.zeroBucket();
        return "{\"scale\":" + h.scale() + ",\"zero\":{\"t\":" + hex(z.zeroThreshold()) + ",\"count\":" + z.count() + ",\"index\":" + z.index()
            + ",\"zscale\":" + z.scale() + "},\"neg\":" + buckets(h.negativeBuckets()) + ",\"pos\":" + buckets(h.positiveBuckets())
            + ",\"sum\":" + hex(h.sum()) + ",\"min\":" + hex(h.min()) + ",\"max\":" + hex(h.max()) + "}";
    }

    static Random r = new Random(20261021);

    static long[] sideIndices(int scale, int n) {
        long[] idx = new long[n];
        double center = r.nextGaussian() * 12;
        long start = scale >= 0 ? (long) (center * Math.scalb(1.0, scale)) : ((long) center) >> -scale;
        long step = Math.max(1, (long) Math.scalb(1.0, Math.max(0, scale - 3)));
        long cur = start;
        for (int i = 0; i < n; i++) {
            cur += 1 + (r.nextInt(4) == 0 ? (long) (r.nextDouble() * step) : 0);
            idx[i] = cur;
        }
        return idx;
    }

    /** A random input as a doc value holds it: threshold-based zero bucket, sorted buckets. */
    static ExponentialHistogram random(StringBuilder spec) {
        int scale = new int[] { -11, -5, -2, -1, 0, 1, 2, 3, 5, 8, 13, 20, 30, 38 }[r.nextInt(14)];
        ExponentialHistogramBuilder b = ExponentialHistogram.builder(scale, NOOP);
        double threshold = 0;
        long zeroCount = 0;
        if (r.nextInt(3) == 0) {
            threshold = new double[] { 0, 1e-300, 1e-6, 0.5, 1, 3, r.nextDouble() * 20 }[r.nextInt(7)];
            zeroCount = r.nextInt(10);
        }
        b.zeroBucket(ZeroBucket.create(threshold, zeroCount));
        StringBuilder neg = new StringBuilder("["), pos = new StringBuilder("[");
        if (r.nextBoolean()) {
            for (long i : sideIndices(scale, r.nextInt(25))) {
                long c = 1 + r.nextInt(r.nextBoolean() ? 3 : 200);
                b.setNegativeBucket(i, c);
                neg.append(neg.length() > 1 ? "," : "").append("[").append(i).append(",").append(c).append("]");
            }
        }
        for (long i : sideIndices(scale, r.nextInt(60))) {
            long c = 1 + r.nextInt(r.nextBoolean() ? 3 : 200);
            b.setPositiveBucket(i, c);
            pos.append(pos.length() > 1 ? "," : "").append("[").append(i).append(",").append(c).append("]");
        }
        String sum = "null", min = "null", max = "null";
        ExponentialHistogram built = b.build();
        if (r.nextBoolean() && built.valueCount() > 0) {
            double s = r.nextGaussian() * 1000;
            built = ExponentialHistogram.builder(built, NOOP).sum(s).build();
            sum = hex(s);
        }
        if (r.nextBoolean() && built.valueCount() > 0) {
            // Bounds a little inside the estimates, as real data has them.
            double mi = built.min() + Math.abs(built.min()) * 1e-3, ma = built.max() - Math.abs(built.max()) * 1e-3;
            if (mi <= ma) {
                built = ExponentialHistogram.builder(built, NOOP).min(mi).max(ma).build();
                min = hex(mi);
                max = hex(ma);
            }
        }
        spec.append("{\"scale\":").append(scale).append(",\"t\":").append(hex(threshold)).append(",\"zc\":").append(zeroCount).append(",\"neg\":")
            .append(neg).append("],\"pos\":").append(pos).append("],\"sum\":").append(sum).append(",\"min\":").append(min).append(",\"max\":").append(max)
            .append(",\"built\":").append(hist(built)).append("}");
        return built;
    }

    static double[] QS = { 0, 0.001, 0.01, 0.1, 0.25, 0.333, 0.5, 0.75, 0.9, 0.99, 0.999, 1 };

    public static void main(String[] args) {
        // Index math at every scale.
        out.append(" \"index\": [");
        boolean first = true;
        for (int scale = ExponentialHistogram.MIN_SCALE; scale <= ExponentialHistogram.MAX_SCALE; scale++) {
            for (int k = 0; k < 40; k++) {
                double v = switch (k % 8) {
                    case 0 -> Math.scalb(1.0, r.nextInt(2000) - 1000);
                    case 1 -> Math.nextUp(Math.scalb(1.0, r.nextInt(200) - 100));
                    case 2 -> Double.MIN_VALUE * (1 + r.nextInt(1000));
                    case 3 -> Math.exp(r.nextGaussian() * 50);
                    case 4 -> Double.MAX_VALUE / (1 + r.nextInt(100));
                    default -> r.nextDouble() * Math.pow(10, r.nextInt(20) - 10);
                };
                out.append(first ? "\n  " : ",\n  ").append("[").append(scale).append(",").append(hex(v)).append(",")
                    .append(ExponentialScaleUtils.computeIndex(v, scale)).append("]");
                first = false;
            }
        }
        out.append("\n ],\n \"bounds\": [");
        first = true;
        for (int scale = ExponentialHistogram.MIN_SCALE; scale <= ExponentialHistogram.MAX_SCALE; scale++) {
            for (int k = 0; k < 25; k++) {
                long maxIdx = scale >= 0 ? (long) Math.min(1L << 61, 1100L * (1L << Math.min(scale, 50))) : 1100L >> -scale;
                long index = (long) ((r.nextDouble() * 2 - 1) * maxIdx);
                out.append(first ? "\n  " : ",\n  ").append("[").append(scale).append(",").append(index).append(",")
                    .append(hex(ExponentialScaleUtils.getLowerBucketBoundary(index, scale))).append(",")
                    .append(hex(ExponentialScaleUtils.getUpperBucketBoundary(index, scale))).append(",")
                    .append(hex(ExponentialScaleUtils.getPointOfLeastRelativeError(index, scale))).append("]");
                first = false;
            }
        }
        out.append("\n ],\n \"merges\": [");
        for (int m = 0; m < 120; m++) {
            int limit = new int[] { 4, 5, 8, 20, 64, 320 }[r.nextInt(6)];
            int n = 1 + r.nextInt(6);
            StringBuilder inputs = new StringBuilder("[");
            ExponentialHistogramMerger merger = ExponentialHistogramMerger.create(limit, NOOP);
            for (int i = 0; i < n; i++) {
                if (i > 0) inputs.append(",");
                merger.add(random(inputs));
            }
            ExponentialHistogram res = merger.get();
            StringBuilder q = new StringBuilder("[");
            for (double x : QS) {
                q.append(q.length() > 1 ? "," : "").append(hex(ExponentialHistogramQuantile.getQuantile(res, x)));
            }
            StringBuilder ranks = new StringBuilder("[");
            double[] probes = { -100, -1, -1e-3, 0, 1e-3, 0.5, 1, 2, 10, 1000, res.min(), res.max(), ExponentialHistogramQuantile.getQuantile(res, 0.5) };
            for (double x : probes) {
                ranks.append(ranks.length() > 1 ? "," : "").append("[").append(hex(x)).append(",").append(ExponentialHistogramQuantile.estimateRank(res, x, false))
                    .append(",").append(ExponentialHistogramQuantile.estimateRank(res, x, true)).append("]");
            }
            out.append(m == 0 ? "\n  " : ",\n  ").append("{\"limit\":").append(limit).append(",\"inputs\":").append(inputs).append("],\"result\":")
                .append(hist(res)).append(",\"quantiles\":").append(q).append("],\"ranks\":").append(ranks).append("]}");
        }
        out.append("\n ],\n \"unions\": [");
        for (int u = 0; u < 30; u++) {
            HistogramUnionState state = HistogramUnionState.create(HistogramUnionState.NOOP_BREAKER, new double[] { 20, 100, 300 }[r.nextInt(3)]);
            StringBuilder vals = new StringBuilder("[");
            int nv = r.nextInt(3) == 0 ? 0 : r.nextInt(1500);
            for (int i = 0; i < nv; i++) {
                double v = r.nextInt(5) == 0 ? 0.0 : r.nextGaussian() * 30 + (r.nextBoolean() ? 50 : -20);
                state.add(v);
                vals.append(i == 0 ? "" : ",").append(hex(v));
            }
            StringBuilder exps = new StringBuilder("[");
            int ne = r.nextInt(3) == 0 ? 0 : 1 + r.nextInt(4);
            for (int i = 0; i < ne; i++) {
                if (i > 0) exps.append(",");
                state.add(random(exps));
            }
            state.compress();
            StringBuilder q = new StringBuilder("[");
            for (double x : QS) {
                q.append(q.length() > 1 ? "," : "").append(hex(state.quantile(x)));
            }
            StringBuilder cdf = new StringBuilder("[");
            for (double x : new double[] { -50, -1, 0, 1, 25, 50, 100 }) {
                cdf.append(cdf.length() > 1 ? "," : "").append("[").append(hex(x)).append(",").append(hex(state.cdf(x))).append("]");
            }
            StringBuilder cs = new StringBuilder("[");
            for (Centroid c : state.centroids()) {
                cs.append(cs.length() > 1 ? "," : "").append("[").append(hex(c.mean())).append(",").append(c.count()).append("]");
            }
            out.append(u == 0 ? "\n  " : ",\n  ").append("{\"compression\":").append(state.compression()).append(",\"values\":").append(vals)
                .append("],\"exps\":").append(exps).append("],\"size\":").append(state.size()).append(",\"min\":").append(hex(state.getMin()))
                .append(",\"max\":").append(hex(state.getMax())).append(",\"quantiles\":").append(q).append("],\"cdf\":").append(cdf)
                .append("],\"centroids\":").append(cs).append("]}");
        }
        out.append("\n ]\n}\n");
        System.out.print(out);
    }
}
