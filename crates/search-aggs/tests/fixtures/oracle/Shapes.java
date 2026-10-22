package org.elasticsearch.xpack.spatial.search.aggregations.bucket.geogrid;

import org.apache.lucene.index.IndexableField;
import org.apache.lucene.util.BytesRef;
import org.elasticsearch.common.geo.GeoBoundingBox;
import org.elasticsearch.common.geo.GeoPoint;
import org.elasticsearch.common.geo.Orientation;
import org.elasticsearch.geometry.*;
import org.elasticsearch.index.mapper.GeoShapeIndexer;
import org.elasticsearch.lucene.spatial.BinaryShapeDocValuesField;
import org.elasticsearch.search.aggregations.bucket.geogrid.GeoTileUtils;
import org.elasticsearch.lucene.spatial.CartesianShapeIndexer;
import org.elasticsearch.lucene.spatial.CoordinateEncoder;
import org.elasticsearch.lucene.spatial.GeometryDocValueReader;
import org.elasticsearch.xpack.spatial.index.fielddata.CartesianShapeValues;
import org.elasticsearch.xpack.spatial.index.fielddata.GeoShapeValues;
import org.elasticsearch.xpack.spatial.index.fielddata.ShapeValues;

import java.util.*;

/**
 * Reference values for `tests/shapes.rs`: Elasticsearch 9.5.5's own shape indexing
 * (`GeoShapeIndexer` / `CartesianShapeIndexer` triangles, `BinaryShapeDocValuesField`'s centroid)
 * and what aggregations read from it (`GeoShapeValue.boundingBox()`, the centroid, and the
 * `GeoTileGridTiler` / `GeoHashGridTiler` cells, unbounded and bounded) for random points,
 * lines, polygons with holes, rectangles and collections, many with vertices on tile and
 * geohash edges. Doubles are raw bit patterns in hex. Compile against the reference node's jars:
 * javac -cp 'lib/*:modules/spatial/*:modules/x-pack-core/*' -d out Shapes.java
 * java -cp 'out:lib/*:modules/spatial/*:modules/x-pack-core/*' org.elasticsearch.xpack.spatial.search.aggregations.bucket.geogrid.Shapes > ../aggs-shapes.json
 */
public class Shapes {
    static Random r = new Random(20261021);

    static String hex(double d) { return "\"" + Long.toHexString(Double.doubleToRawLongBits(d)) + "\""; }

    /** A coordinate, snapped half the time to tile or geohash edges or whole degrees. */
    static double snap(double v, boolean lat) {
        switch (r.nextInt(6)) {
            case 0: {
                int z = 1 + r.nextInt(8);
                int tiles = 1 << z;
                if (lat) {
                    int y = (int) Math.round((1 - Math.log(Math.tan(Math.toRadians(v)) + 1 / Math.cos(Math.toRadians(v))) / Math.PI) / 2 * tiles);
                    y = Math.max(1, Math.min(tiles - 1, y));
                    return GeoTileUtils.tileToLat(y, tiles);
                }
                return GeoTileUtils.tileToLon((int) Math.round((v + 180) / 360 * tiles), tiles);
            }
            case 1: {
                double w = lat ? 180.0 / (1 << (2 + r.nextInt(8))) : 360.0 / (1 << (3 + r.nextInt(8)));
                return Math.round(v / w) * w;
            }
            case 2:
                return Math.rint(v);
            default:
                return v;
        }
    }

    static double[][] star(double cx, double cy, double rad, int n, boolean ccw) {
        double[] angles = new double[n];
        for (int i = 0; i < n; i++) angles[i] = r.nextDouble() * 2 * Math.PI;
        Arrays.sort(angles);
        double[] xs = new double[n + 1], ys = new double[n + 1];
        for (int i = 0; i < n; i++) {
            double a = ccw ? angles[i] : -angles[i];
            double d = rad * (0.6 + 0.4 * r.nextDouble());
            xs[i] = Math.max(-179.9, Math.min(179.9, cx + d * Math.cos(a)));
            ys[i] = Math.max(-89.9, Math.min(89.9, cy + d * Math.sin(a)));
        }
        xs[n] = xs[0];
        ys[n] = ys[0];
        return new double[][] { xs, ys };
    }

    static String coords(double[] xs, double[] ys) {
        StringBuilder s = new StringBuilder("[");
        for (int i = 0; i < xs.length; i++) s.append(i == 0 ? "" : ",").append("[").append(hex(xs[i])).append(",").append(hex(ys[i])).append("]");
        return s.append("]").toString();
    }

    static Polygon polygon(double cx, double cy, double rad, StringBuilder js) {
        double[][] shell = star(cx, cy, rad, 3 + r.nextInt(12), true);
        List<LinearRing> holes = new ArrayList<>();
        js.append("{\"type\":\"polygon\",\"shell\":").append(coords(shell[0], shell[1])).append(",\"holes\":[");
        if (r.nextInt(3) == 0) {
            double[][] h = star(cx, cy, rad * 0.25, 3 + r.nextInt(6), false);
            holes.add(new LinearRing(h[0], h[1]));
            js.append(coords(h[0], h[1]));
        }
        js.append("]}");
        return new Polygon(new LinearRing(shell[0], shell[1]), holes);
    }

    static Geometry random(StringBuilder js, boolean cartesian) {
        double cx = snap((r.nextDouble() * 2 - 1) * 170, false), cy = snap((r.nextDouble() * 2 - 1) * 80, true);
        double rad = new double[] { 0.01, 0.3, 2, 8, 25 }[r.nextInt(5)];
        switch (r.nextInt(cartesian ? 5 : 7)) {
            case 0:
                js.append("{\"type\":\"point\",\"c\":[").append(hex(cx)).append(",").append(hex(cy)).append("]}");
                return new Point(cx, cy);
            case 1: {
                int n = 2 + r.nextInt(6);
                double[] xs = new double[n], ys = new double[n];
                for (int i = 0; i < n; i++) {
                    xs[i] = Math.max(-179.9, Math.min(179.9, snap(cx + (r.nextDouble() * 2 - 1) * rad, false)));
                    ys[i] = Math.max(-89.9, Math.min(89.9, snap(cy + (r.nextDouble() * 2 - 1) * rad, true)));
                }
                js.append("{\"type\":\"line\",\"c\":").append(coords(xs, ys)).append("}");
                return new Line(xs, ys);
            }
            case 2:
                return polygon(cx, cy, rad, js);
            case 3: {
                double w = Math.min(rad, 170 - Math.abs(cx)), h = Math.min(rad, 85 - Math.abs(cy));
                double minX = snap(cx - w, false), maxX = snap(cx + w, false), minY = snap(cy - h, true), maxY = snap(cy + h, true);
                if (minX >= maxX || minY >= maxY) {
                    js.append("{\"type\":\"point\",\"c\":[").append(hex(cx)).append(",").append(hex(cy)).append("]}");
                    return new Point(cx, cy);
                }
                js.append("{\"type\":\"rect\",\"c\":[").append(hex(minX)).append(",").append(hex(maxX)).append(",").append(hex(minY)).append(",").append(hex(maxY)).append("]}");
                return new Rectangle(minX, maxX, maxY, minY);
            }
            case 4: {
                int n = 1 + r.nextInt(5);
                double[] xs = new double[n], ys = new double[n];
                for (int i = 0; i < n; i++) {
                    xs[i] = Math.max(-179.9, Math.min(179.9, snap(cx + (r.nextDouble() * 2 - 1) * rad, false)));
                    ys[i] = Math.max(-89.9, Math.min(89.9, snap(cy + (r.nextDouble() * 2 - 1) * rad, true)));
                }
                List<Point> pts = new ArrayList<>();
                for (int i = 0; i < n; i++) pts.add(new Point(xs[i], ys[i]));
                js.append("{\"type\":\"multipoint\",\"c\":").append(coords(xs, ys)).append("}");
                return new MultiPoint(pts);
            }
            default: {
                int n = 2 + r.nextInt(3);
                List<Geometry> parts = new ArrayList<>();
                js.append("{\"type\":\"collection\",\"parts\":[");
                for (int i = 0; i < n; i++) {
                    if (i > 0) js.append(",");
                    double px = Math.max(-170, Math.min(170, cx + (r.nextDouble() * 2 - 1) * rad * 3));
                    double py = Math.max(-80, Math.min(80, cy + (r.nextDouble() * 2 - 1) * rad * 3));
                    if (r.nextBoolean()) {
                        parts.add(polygon(px, py, rad / 2, js));
                    } else {
                        js.append("{\"type\":\"point\",\"c\":[").append(hex(px)).append(",").append(hex(py)).append("]}");
                        parts.add(new Point(px, py));
                    }
                }
                js.append("]}");
                return new GeometryCollection<>(parts);
            }
        }
    }

    static String bbox(ShapeValues.BoundingBox b) {
        return "[" + hex(b.top) + "," + hex(b.bottom) + "," + hex(b.negLeft) + "," + hex(b.negRight) + "," + hex(b.posLeft) + "," + hex(b.posRight) + "]";
    }

    static String cells(GeoGridTiler tiler, GeoShapeValues.GeoShapeValue v) throws Exception {
        GeoShapeCellValues values = new GeoShapeCellValues(null, tiler, l -> {});
        int n = tiler.setValues(values, v);
        long[] c = Arrays.copyOf(values.getValues(), n);
        Arrays.sort(c);
        if (c.length > 200) {
            // Large cell sets as a checksum: count, wrapping sum, xor.
            long sum = 0, xor = 0;
            for (long x : c) {
                sum += x;
                xor ^= x;
            }
            return "{\"n\":" + c.length + ",\"sum\":" + sum + ",\"xor\":" + xor + "}";
        }
        StringBuilder s = new StringBuilder("[");
        for (int i = 0; i < c.length; i++) s.append(i == 0 ? "" : ",").append(c[i]);
        return s.append("]").toString();
    }

    public static void main(String[] args) throws Exception {
        GeoBoundingBox[] boxes = {
            new GeoBoundingBox(new GeoPoint(40, -20), new GeoPoint(-10, 60)),
            new GeoBoundingBox(new GeoPoint(70, 150), new GeoPoint(10, -160)),
            new GeoBoundingBox(new GeoPoint(5.5, 1.25), new GeoPoint(1.0, 7.0)) };
        StringBuilder out = new StringBuilder("{\n \"boxes\": [");
        for (int i = 0; i < boxes.length; i++) {
            out.append(i == 0 ? "" : ",").append("[").append(hex(boxes[i].top())).append(",").append(hex(boxes[i].left())).append(",")
                .append(hex(boxes[i].bottom())).append(",").append(hex(boxes[i].right())).append("]");
        }
        out.append("],\n \"geo\": [");
        int[] tilePrecisions = { 0, 1, 2, 3, 5, 7, 9 };
        int[] hashPrecisions = { 1, 2, 3, 4 };
        for (int k = 0; k < 260; k++) {
            StringBuilder js;
            Geometry g;
            List<IndexableField> fields;
            while (true) {
                js = new StringBuilder();
                g = random(js, false);
                try {
                    fields = new GeoShapeIndexer(Orientation.CCW, "f").indexShape(g);
                    break;
                } catch (IllegalArgumentException e) {
                    // self-intersecting random polygon: draw another
                }
            }
            BinaryShapeDocValuesField dv = new BinaryShapeDocValuesField("f", CoordinateEncoder.GEO);
            dv.add(fields, g);
            BytesRef bytes = dv.binaryValue();
            GeometryDocValueReader reader = new GeometryDocValueReader();
            reader.reset(bytes);
            GeoShapeValues.GeoShapeValue v = new GeoShapeValues.GeoShapeValue();
            v.reset(bytes);
            StringBuilder tris = new StringBuilder("[");
            for (int i = 0; i < fields.size(); i++) {
                BytesRef b = fields.get(i).binaryValue();
                tris.append(i == 0 ? "\"" : ",\"").append(HexFormat.of().formatHex(b.bytes, b.offset, b.length)).append("\"");
            }
            StringBuilder tiles = new StringBuilder("[");
            for (int p : tilePrecisions) {
                for (int bi = -1; bi < boxes.length; bi++) {
                    if (bi >= 0 && r.nextInt(3) != 0) continue;
                    tiles.append(tiles.length() > 1 ? "," : "").append("[").append(p).append(",").append(bi).append(",")
                        .append(cells(GeoTileGridTiler.makeGridTiler(p, bi < 0 ? null : boxes[bi]), v)).append("]");
                }
            }
            StringBuilder hexes = new StringBuilder("[");
            for (int p : new int[] { 0, 1, 2, 3, 4 }) {
                for (int bi = -1; bi < boxes.length; bi++) {
                    if (bi >= 0 && r.nextInt(3) != 0) continue;
                    hexes.append(hexes.length() > 1 ? "," : "").append("[").append(p).append(",").append(bi).append(",")
                        .append(cells(GeoHexGridTiler.makeGridTiler(p, bi < 0 ? null : boxes[bi]), v)).append("]");
                }
            }
            StringBuilder hashes = new StringBuilder("[");
            for (int p : hashPrecisions) {
                for (int bi = -1; bi < boxes.length; bi++) {
                    if (bi >= 0 && r.nextInt(3) != 0) continue;
                    hashes.append(hashes.length() > 1 ? "," : "").append("[").append(p).append(",").append(bi).append(",")
                        .append(cells(GeoHashGridTiler.makeGridTiler(p, bi < 0 ? null : boxes[bi]), v)).append("]");
                }
            }
            out.append(k == 0 ? "\n  " : ",\n  ").append("{\"geom\":").append(js).append(",\"triangles\":").append(tris).append("],\"centroid\":[")
                .append(reader.getCentroidX()).append(",").append(reader.getCentroidY()).append(",").append(reader.getDimensionalShapeType().ordinal())
                .append(",").append(hex(reader.getSumCentroidWeight())).append("],\"bbox\":").append(bbox(v.boundingBox())).append(",\"tiles\":")
                .append(tiles).append("],\"hashes\":").append(hashes).append("],\"hexes\":").append(hexes).append("]}");
        }
        out.append("\n ],\n \"h3\": [");
        for (int k = 0; k < 300; k++) {
            int res = k % 8;
            long h = org.elasticsearch.h3.H3.geoToH3((r.nextDouble() * 2 - 1) * 89, (r.nextDouble() * 2 - 1) * 179, res);
            if (k < 24) h = org.elasticsearch.h3.H3.getLongRes0Cells()[(k * 5) % 122];
            if (k >= 24 && k < 40) h = org.elasticsearch.h3.H3.northPolarH3(k % 16);
            StringBuilder e = new StringBuilder("[" + h + ",[");
            long[] ring = org.elasticsearch.h3.H3.hexRing(h);
            for (int i = 0; i < ring.length; i++) e.append(i == 0 ? "" : ",").append(ring[i]);
            e.append("],[");
            int rs = org.elasticsearch.h3.H3.getResolution(h);
            if (rs < 15) {
                long[] ch = org.elasticsearch.h3.H3.h3ToChildren(h);
                for (int i = 0; i < ch.length; i++) e.append(i == 0 ? "" : ",").append(ch[i]);
                e.append("],[");
                long[] nc = org.elasticsearch.h3.H3.h3ToNoChildrenIntersecting(h);
                for (int i = 0; i < nc.length; i++) e.append(i == 0 ? "" : ",").append(nc[i]);
            } else {
                e.append("],[");
            }
            e.append("],").append(rs > 0 ? org.elasticsearch.h3.H3.h3ToParent(h) : -1).append("]");
            out.append(k == 0 ? "\n  " : ",\n  ").append(e);
        }
        out.append("\n ],\n \"cartesian\": [");
        for (int k = 0; k < 60; k++) {
            StringBuilder js;
            Geometry g;
            List<IndexableField> fields;
            while (true) {
                js = new StringBuilder();
                g = random(js, true);
                try {
                    fields = new CartesianShapeIndexer("f").indexShape(g);
                    break;
                } catch (IllegalArgumentException e) {
                    // self-intersecting random polygon: draw another
                }
            }
            BinaryShapeDocValuesField dv = new BinaryShapeDocValuesField("f", CoordinateEncoder.CARTESIAN);
            dv.add(fields, g);
            BytesRef bytes = dv.binaryValue();
            GeometryDocValueReader reader = new GeometryDocValueReader();
            reader.reset(bytes);
            CartesianShapeValues.CartesianShapeValue v = new CartesianShapeValues.CartesianShapeValue();
            v.reset(bytes);
            StringBuilder tris = new StringBuilder("[");
            for (int i = 0; i < fields.size(); i++) {
                BytesRef b = fields.get(i).binaryValue();
                tris.append(i == 0 ? "\"" : ",\"").append(HexFormat.of().formatHex(b.bytes, b.offset, b.length)).append("\"");
            }
            out.append(k == 0 ? "\n  " : ",\n  ").append("{\"geom\":").append(js).append(",\"triangles\":").append(tris).append("],\"centroid\":[")
                .append(reader.getCentroidX()).append(",").append(reader.getCentroidY()).append(",").append(reader.getDimensionalShapeType().ordinal())
                .append(",").append(hex(reader.getSumCentroidWeight())).append("],\"bbox\":").append(bbox(v.boundingBox())).append("}");
        }
        out.append("\n ]\n}\n");
        System.out.print(out);
    }
}
