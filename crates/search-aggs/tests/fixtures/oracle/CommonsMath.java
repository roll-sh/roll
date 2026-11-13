import org.apache.commons.math3.distribution.TDistribution;
import org.apache.commons.math3.special.Beta;
import org.apache.commons.math3.special.Gamma;
import org.apache.commons.math3.util.FastMath;
import java.util.*;
import java.util.function.*;

/**
 * Reference outputs for `tests/commons_math.rs`: Commons Math 3.6.1 (the jar the reference
 * node's x-pack-analytics module ships) on deterministic inputs. Every number is a raw bit
 * pattern in hex. Regenerate:
 * java -cp commons-math3-3.6.1.jar CommonsMath.java > ../aggs-commons-math.json
 */
public class CommonsMath {
    static StringBuilder out = new StringBuilder("{\n");
    static String hex(double d) { return "\"" + Long.toHexString(Double.doubleToRawLongBits(d)) + "\""; }

    static void unary(String name, double[] xs, DoubleUnaryOperator f, boolean last) {
        out.append(" \"").append(name).append("\": [");
        for (int i = 0; i < xs.length; i++) {
            out.append(i == 0 ? "" : ",").append("\n  [").append(hex(xs[i])).append(", ").append(hex(f.applyAsDouble(xs[i]))).append("]");
        }
        out.append("\n ]").append(last ? "\n" : ",\n");
    }

    interface F3 { double apply(double a, double b, double c); }

    static void ternary(String name, double[][] xs, F3 f, boolean last) {
        out.append(" \"").append(name).append("\": [");
        for (int i = 0; i < xs.length; i++) {
            double[] x = xs[i];
            double r;
            try { r = f.apply(x[0], x[1], x[2]); } catch (RuntimeException e) { r = Double.NaN; }
            out.append(i == 0 ? "" : ",").append("\n  [").append(hex(x[0])).append(", ").append(hex(x[1])).append(", ").append(hex(x[2])).append(", ").append(hex(r)).append("]");
        }
        out.append("\n ]").append(last ? "\n" : ",\n");
    }

    public static void main(String[] args) {
        Random r = new Random(20251021);
        double[] e = new double[600];
        for (int i = 0; i < e.length; i++) {
            switch (i % 4) {
                case 0 -> e[i] = -745 + r.nextDouble() * 1454;
                case 1 -> e[i] = (r.nextDouble() - 0.5) * 40;
                case 2 -> e[i] = -r.nextDouble() * 745;
                default -> e[i] = (r.nextDouble() - 0.5) * 1e-3;
            }
        }
        e[0] = 0; e[1] = -0.0; e[2] = 709.7; e[3] = -745.5; e[4] = -709.2; e[5] = -3.0; e[6] = 1e-300; e[7] = 710;
        unary("exp", e, FastMath::exp, false);
        double[] l = new double[600];
        for (int i = 0; i < l.length; i++) {
            switch (i % 4) {
                case 0 -> l[i] = Math.pow(10, (r.nextDouble() - 0.5) * 600);
                case 1 -> l[i] = 0.99 + r.nextDouble() * 0.02;
                case 2 -> l[i] = r.nextDouble() * 10;
                default -> l[i] = Double.MIN_VALUE * (1 + r.nextInt(1 << 20));
            }
        }
        l[0] = 1; l[1] = 2; l[2] = Math.E; l[3] = 0.5; l[4] = Double.MAX_VALUE; l[5] = 2 * Math.PI;
        unary("log", l, FastMath::log, false);
        double[] lp = new double[600];
        for (int i = 0; i < lp.length; i++) {
            switch (i % 3) {
                case 0 -> lp[i] = -1 + r.nextDouble() * 11;
                case 1 -> lp[i] = (r.nextDouble() - 0.5) * 4e-6;
                default -> lp[i] = -r.nextDouble();
            }
        }
        unary("log1p", lp, FastMath::log1p, false);
        double[] g = new double[400];
        for (int i = 0; i < g.length; i++) g[i] = i % 2 == 0 ? r.nextDouble() * 12 : r.nextDouble() * 1000;
        unary("logGamma", g, Gamma::logGamma, false);
        double[][] b = new double[600][];
        double[] scales = {0.3, 1.5, 3, 9, 30, 2000};
        for (int i = 0; i < b.length; i++) {
            double p = r.nextDouble() * scales[r.nextInt(scales.length)] + 1e-3;
            double q = i % 3 == 0 ? 0.5 : r.nextDouble() * scales[r.nextInt(scales.length)] + 1e-3;
            b[i] = new double[] { p, q, 0 };
        }
        ternary("logBeta", b, (p, q, unused) -> Beta.logBeta(p, q), false);
        double[][] rb = new double[600][];
        for (int i = 0; i < rb.length; i++) {
            double a = r.nextDouble() * scales[r.nextInt(scales.length)] + 1e-2;
            double bb = i % 2 == 0 ? 0.5 : r.nextDouble() * scales[r.nextInt(scales.length)] + 1e-2;
            rb[i] = new double[] { r.nextDouble(), a, bb };
        }
        ternary("regularizedBeta", rb, Beta::regularizedBeta, false);
        double[][] t = new double[800][];
        for (int i = 0; i < t.length; i++) {
            double dof = switch (i % 4) {
                case 0 -> 1 + r.nextInt(60);
                case 1 -> 1 + r.nextDouble() * 100;
                case 2 -> 100 + r.nextDouble() * 5000;
                default -> 0.2 + r.nextDouble() * 3;
            };
            double x = -r.nextDouble() * (i % 5 == 0 ? 60 : 6);
            t[i] = new double[] { dof, x, 0 };
        }
        ternary("tCdf", t, (dof, x, unused) -> new TDistribution(dof).cumulativeProbability(x), true);
        out.append("}\n");
        System.out.print(out);
    }
}
