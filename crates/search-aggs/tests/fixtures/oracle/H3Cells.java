import org.elasticsearch.common.geo.GeoBoundingBox;
import org.elasticsearch.common.geo.GeoPoint;
import org.elasticsearch.h3.CellBoundary;
import org.elasticsearch.h3.H3;
import org.elasticsearch.h3.LatLng;
import org.elasticsearch.xpack.spatial.common.H3SphericalUtil;

import java.util.*;

/**
 * Reference cells for `tests/h3.rs`: Elasticsearch 9.5.5's `H3.geoToH3` for random points at
 * every resolution, and for many cells (all base cells, pentagon children, random cells) the
 * boundary vertices and the bounding box `geohex_grid`'s `bounds` filter uses. Doubles are bit
 * patterns. Classpath: the reference node's `lib/`, `modules/spatial/` and `modules/x-pack-core/`.
 * java -cp 'lib/*:modules/spatial/*:modules/x-pack-core/*' H3Cells.java > ../aggs-h3-cells.json
 */
public class H3Cells {
    static String hex(double d) { return "\"" + Long.toHexString(Double.doubleToRawLongBits(d)) + "\""; }

    public static void main(String[] args) {
        Random r = new Random(31);
        StringBuilder out = new StringBuilder("{\n \"points\": [");
        List<double[]> pts = new ArrayList<>();
        double[] lats = { 90, -90, 0, 89.99999, -89.99999, 45, -45 };
        double[] lons = { 180, -180, 0, 179.99999, -179.99999, 90, -90 };
        for (double la : lats) for (double lo : lons) pts.add(new double[] { la, lo });
        for (int i = 0; i < 2400; i++) {
            double la = i % 5 == 0 ? (r.nextDouble() * 2 - 1) * 90 : Math.toDegrees(Math.asin(r.nextDouble() * 2 - 1));
            double lo = r.nextDouble() * 360 - 180;
            if (i % 9 == 0) { la = Math.round(la * 100) / 100.0; lo = Math.round(lo * 100) / 100.0; }
            pts.add(new double[] { la, lo });
        }
        List<Long> cells = new ArrayList<>();
        for (int i = 0; i < pts.size(); i++) {
            double[] p = pts.get(i);
            int res = i % 16;
            long h = H3.geoToH3(p[0], p[1], res);
            if (i % 4 == 0) cells.add(h);
            out.append(i == 0 ? "" : ",").append("\n  [").append(hex(p[0])).append(", ").append(hex(p[1])).append(", ").append(res).append(", \"").append(H3.h3ToString(h)).append("\"]");
        }
        for (long c : H3.getLongRes0Cells()) {
            cells.add(c);
            if (H3.isPentagon(c)) {
                long[] kids = H3.h3ToChildren(c);
                for (long k : kids) { cells.add(k); for (long g : H3.h3ToChildren(k)) cells.add(g); }
            }
        }
        for (int res = 0; res <= 15; res++) { cells.add(H3.northPolarH3(res)); cells.add(H3.southPolarH3(res)); }
        out.append("\n ],\n \"cells\": [");
        GeoBoundingBox bbox = new GeoBoundingBox(new GeoPoint(), new GeoPoint());
        for (int i = 0; i < cells.size(); i++) {
            long c = cells.get(i);
            CellBoundary b = H3.h3ToGeoBoundary(c);
            StringBuilder v = new StringBuilder();
            for (int k = 0; k < b.numPoints(); k++) {
                LatLng ll = b.getLatLon(k);
                v.append(k == 0 ? "" : ", ").append(hex(ll.getLatRad())).append(", ").append(hex(ll.getLonRad()));
            }
            H3SphericalUtil.computeGeoBounds(c, bbox);
            LatLng center = H3.h3ToLatLng(c);
            out.append(i == 0 ? "" : ",").append("\n  {\"h\": \"").append(H3.h3ToString(c)).append("\", \"pentagon\": ").append(H3.isPentagon(c))
                .append(", \"center\": [").append(hex(center.getLatRad())).append(", ").append(hex(center.getLonRad())).append("]")
                .append(", \"boundary\": [").append(v).append("], \"bounds\": [").append(hex(bbox.top())).append(", ").append(hex(bbox.left()))
                .append(", ").append(hex(bbox.bottom())).append(", ").append(hex(bbox.right())).append("]}");
        }
        out.append("\n ]\n}\n");
        System.out.print(out);
    }
}
