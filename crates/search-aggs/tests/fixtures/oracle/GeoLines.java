package org.elasticsearch.xpack.spatial.search.aggregations;

import org.apache.lucene.index.LeafReaderContext;
import org.elasticsearch.common.util.BigArrays;
import org.elasticsearch.index.fielddata.SortedBinaryDocValues;
import org.elasticsearch.index.fielddata.SortedNumericDoubleValues;
import org.elasticsearch.index.fielddata.SortedNumericLongValues;
import org.elasticsearch.search.aggregations.support.ValuesSource;
import org.elasticsearch.search.sort.BucketedSort;
import org.elasticsearch.search.sort.SortOrder;
import org.elasticsearch.xpack.spatial.search.aggregations.support.GeoLineMultiValuesSource;

import java.util.*;

/**
 * Reference lines for `tests/geo_line.rs`: Elasticsearch 9.5.5's own `GeoLineBucketedSort`
 * (gather, heap, `PathArraySorter`) fed one bucket of documents with tied sort values and
 * missing points. Compile against the reference node's `lib/` and `modules/spatial/` jars:
 * javac -cp 'lib/*:modules/spatial/*:modules/x-pack-core/*' -d out GeoLines.java
 * java -cp 'out:lib/*:modules/spatial/*:modules/x-pack-core/*' org.elasticsearch.xpack.spatial.search.aggregations.GeoLines > ../aggs-geo-line.json
 */
public class GeoLines {
    /** Production-like: a page recycler (arrays of 1024+ longs are paged). */
    static final BigArrays ARRAYS = new BigArrays(new org.elasticsearch.common.util.PageCacheRecycler(org.elasticsearch.common.settings.Settings.EMPTY), null, "request");

    static final class Source extends GeoLineMultiValuesSource {
        Source(Double[] sorts, Long[] points) {
            super(Map.of());
            values = new HashMap<>();
            values.put("sort", new ValuesSource.Numeric() {
                public boolean isFloatingPoint() { return true; }
                public SortedNumericLongValues longValues(LeafReaderContext c) { throw new UnsupportedOperationException(); }
                public SortedBinaryDocValues bytesValues(LeafReaderContext c) { throw new UnsupportedOperationException(); }
                public SortedNumericDoubleValues doubleValues(LeafReaderContext c) {
                    return new SortedNumericDoubleValues(null) {
                        int doc;
                        boolean read;
                        public boolean advanceExact(int d) { doc = d; read = false; return sorts[d] != null; }
                        public double nextValue() { return sorts[doc]; }
                        public int docValueCount() { return 1; }
                    };
                }
            });
            values.put("point", new ValuesSource.GeoPoint() {
                public SortedBinaryDocValues bytesValues(LeafReaderContext c) { throw new UnsupportedOperationException(); }
                public SortedNumericLongValues geoSortedNumericDocValues(LeafReaderContext c) {
                    return new SortedNumericLongValues(null) {
                        int doc;
                        public boolean advanceExact(int d) { doc = d; return points[d] != null; }
                        public int docValueCount() { return 1; }
                        public long nextValue() { return points[doc]; }
                    };
                }
            });
        }
    }

    public static void main(String[] args) throws Exception {
        Random r = new Random(2024);
        StringBuilder out = new StringBuilder("[\n");
        for (int c = 0; c < 306; c++) {
            boolean big = c >= 300;
            int n = big ? 1000 + r.nextInt(1200) : r.nextInt(c % 4 == 0 ? 200 : 40);
            int size = big ? 900 + r.nextInt(300) : 1 + r.nextInt(c % 3 == 0 ? 30 : 8);
            SortOrder order = r.nextBoolean() ? SortOrder.ASC : SortOrder.DESC;
            int distinct = 1 + r.nextInt(c % 2 == 0 ? 4 : 50);
            Double[] sorts = new Double[n];
            Long[] points = new Long[n];
            StringBuilder docs = new StringBuilder();
            for (int i = 0; i < n; i++) {
                sorts[i] = r.nextInt(8) == 0 ? null : (double) (r.nextInt(distinct) - distinct / 3);
                Integer lat = null, lon = null;
                if (r.nextInt(6) != 0) {
                    lat = r.nextInt();
                    lon = r.nextInt();
                    points[i] = (((long) lat) << 32) | (lon & 0xffffffffL);
                }
                docs.append(i == 0 ? "" : ", ").append("[").append(sorts[i]).append(", ").append(lat).append(", ").append(lon).append("]");
            }
            Source src = new Source(sorts, points);
            GeoLineBucketedSort.Extra extra = new GeoLineBucketedSort.Extra(ARRAYS, src);
            GeoLineBucketedSort sort = new GeoLineBucketedSort(ARRAYS, order, null, size, src, extra);
            BucketedSort.Leaf leaf = sort.forLeaf(null);
            String error = null;
            try {
                for (int i = 0; i < n; i++) {
                    leaf.collect(i, 0);
                }
            } catch (ArrayIndexOutOfBoundsException e) {
                error = e.getMessage();
            }
            if (error != null) {
                out.append(c == 0 ? "" : ",\n").append(" {\"size\": ").append(size).append(", \"desc\": ").append(order == SortOrder.DESC)
                    .append(", \"docs\": [").append(docs).append("], \"error\": \"").append(error).append("\"}");
                continue;
            }
            InternalGeoLine line = sort.buildAggregation(0, "g", null, n <= size, true, size, b -> b);
            Map<String, Object> geo = line.geoJSONGeometry();
            StringBuilder coords = new StringBuilder();
            Object cs = geo.get("coordinates");
            if (cs instanceof double[] one) {
                coords.append(Arrays.toString(one));
            } else {
                Object[] all = (Object[]) cs;
                coords.append("[");
                for (int i = 0; i < all.length; i++) coords.append(i == 0 ? "" : ", ").append(Arrays.toString((double[]) all[i]));
                coords.append("]");
            }
            out.append(c == 0 ? "" : ",\n").append(" {\"size\": ").append(size).append(", \"desc\": ").append(order == SortOrder.DESC)
                .append(", \"docs\": [").append(docs).append("], \"line\": ").append(Arrays.toString(line.line()))
                .append(", \"sort_values\": ").append(Arrays.toString(line.sortVals())).append(", \"type\": \"").append(geo.get("type"))
                .append("\", \"coordinates\": ").append(coords).append("}");
        }
        out.append("\n]\n");
        System.out.print(out);
    }
}
