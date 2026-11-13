import org.elasticsearch.core.ESSloppyMath;
import org.elasticsearch.search.aggregations.bucket.geogrid.GeoTileUtils;

import java.util.Random;

/**
 * Reference outputs for `tests/geo_math.rs`: `StrictMath.expm1`, Elasticsearch's `ESSloppyMath`
 * `sinh` / `atan` (core `FastMath`) and `GeoTileUtils.tileToLat` at every zoom (including the
 * boundary snapping from 2^20 tiles on). Doubles are raw bit patterns in hex. Run against the
 * reference node's `lib/` jars:
 * javac -cp 'lib/*' -d out GeoMath.java
 * java -cp 'out:lib/*' GeoMath > ../aggs-geo-math.json
 */
public class GeoMath {
    static String hex(double d) { return "\"" + Long.toHexString(Double.doubleToRawLongBits(d)) + "\""; }

    public static void main(String[] args) {
        Random r = new Random(5);
        StringBuilder out = new StringBuilder("{\n \"tile_to_lat\": [");
        boolean first = true;
        for (int z = 0; z <= 29; z++) {
            int tiles = 1 << z;
            for (int k = 0; k < 50; k++) {
                int y = k < 3 ? k : (k < 6 ? tiles - (k - 3) : r.nextInt(tiles + 1));
                y = Math.max(0, Math.min(tiles, y));
                out.append(first ? "\n  " : ",\n  ").append("[").append(z).append(",").append(y).append(",").append(hex(GeoTileUtils.tileToLat(y, tiles))).append("]");
                first = false;
            }
        }
        out.append("\n ],\n \"fns\": [");
        for (int i = 0; i < 3000; i++) {
            double v = switch (i % 6) {
                case 0 -> (r.nextDouble() * 2 - 1) * 4;
                case 1 -> (r.nextDouble() * 2 - 1) * 21;
                case 2 -> (r.nextDouble() * 2 - 1) * 1e-9;
                case 3 -> (r.nextDouble() * 2 - 1) * 0.4;
                case 4 -> (r.nextDouble() * 2 - 1) * 700;
                default -> (r.nextDouble() * 2 - 1) * 1e5;
            };
            out.append(i == 0 ? "\n  " : ",\n  ").append("[").append(hex(v)).append(",").append(hex(StrictMath.expm1(v))).append(",")
                .append(hex(Math.abs(v) < 22 ? ESSloppyMath.sinh(v) : Double.NaN)).append(",").append(hex(ESSloppyMath.atan(v))).append("]");
        }
        out.append("\n ]\n}\n");
        System.out.print(out);
    }
}
