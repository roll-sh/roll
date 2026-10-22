// Lucene oracle for search-index's tessellator: each input line is a polygon
// "lon lat,lon lat,...[|hole ring|hole ring]" (rings closed); prints one line per polygon:
// "OK <n> ax ay bx by cx cy ab bc ca;..." (encoded ints) or "ERR <message>".
import java.nio.file.*;
import java.util.*;
import org.apache.lucene.geo.*;

public class TessOracle {
    static double[][] ring(String s) {
        String[] pts = s.trim().split(",");
        double[] lats = new double[pts.length], lons = new double[pts.length];
        for (int i = 0; i < pts.length; i++) {
            String[] p = pts[i].trim().split(" ");
            lons[i] = Double.parseDouble(p[0]);
            lats[i] = Double.parseDouble(p[1]);
        }
        return new double[][] {lats, lons};
    }
    public static void main(String[] args) throws Exception {
        boolean check = args.length > 1 && args[1].equals("check");
        StringBuilder out = new StringBuilder();
        for (String line : Files.readAllLines(Paths.get(args[0]))) {
            try {
                String[] rings = line.split("\\|");
                Polygon[] holes = new Polygon[rings.length - 1];
                for (int i = 1; i < rings.length; i++) {
                    double[][] r = ring(rings[i]);
                    holes[i - 1] = new Polygon(r[0], r[1]);
                }
                double[][] r = ring(rings[0]);
                Polygon p = new Polygon(r[0], r[1], holes);
                List<Tessellator.Triangle> ts = Tessellator.tessellate(p, check);
                out.append("OK ").append(ts.size()).append(' ');
                for (Tessellator.Triangle t : ts) {
                    for (int v = 0; v < 3; v++) out.append(t.getEncodedX(v)).append(' ').append(t.getEncodedY(v)).append(' ');
                    for (int v = 0; v < 3; v++) out.append(t.isEdgefromPolygon(v) ? 1 : 0).append(v < 2 ? " " : "");
                    out.append(';');
                }
                out.append('\n');
            } catch (Exception e) {
                out.append("ERR ").append(e.getMessage()).append('\n');
            }
        }
        System.out.print(out);
    }
}
