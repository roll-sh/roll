package org.elasticsearch.h3;

import java.util.*;
import java.util.function.DoubleUnaryOperator;

/**
 * Reference outputs for `tests/h3.rs` (math part): `StrictMath` (fdlibm) and the H3 library's
 * own `FastMath` on deterministic inputs, as raw bit patterns. Compile against the reference
 * node's `modules/spatial/elasticsearch-h3-9.5.5.jar`:
 * javac -cp elasticsearch-h3-9.5.5.jar -d out H3Math.java
 * java -cp out:elasticsearch-h3-9.5.5.jar org.elasticsearch.h3.H3Math > ../aggs-h3-math.json
 */
public class H3Math {
    static StringBuilder out = new StringBuilder("{\n");
    static String hex(double d) { return "\"" + Long.toHexString(Double.doubleToRawLongBits(d)) + "\""; }

    static void unary(String name, double[] xs, DoubleUnaryOperator f) {
        out.append(" \"").append(name).append("\": [");
        for (int i = 0; i < xs.length; i++) {
            out.append(i == 0 ? "" : ",").append("[").append(hex(xs[i])).append(",").append(hex(f.applyAsDouble(xs[i]))).append("]");
        }
        out.append("],\n");
    }

    public static void main(String[] args) {
        Random r = new Random(9);
        int n = 1000;
        double[] angles = new double[n], small = new double[n], unit = new double[n], wide = new double[n], pows = new double[n];
        for (int i = 0; i < n; i++) {
            angles[i] = switch (i % 5) {
                case 0 -> (r.nextDouble() - 0.5) * 4 * Math.PI;
                case 1 -> (r.nextDouble() - 0.5) * 1e4;
                case 2 -> (r.nextDouble() - 0.5) * 1e7;
                case 3 -> (r.nextDouble() - 0.5) * 1e-6;
                default -> r.nextInt(64) * Math.PI / 32 + (r.nextDouble() - 0.5) * 1e-12;
            };
            small[i] = (r.nextDouble() - 0.5) * Math.PI * (i % 3 == 0 ? 0.5 : 1.0);
            unit[i] = i % 4 == 0 ? 1 - r.nextDouble() * 1e-3 : (r.nextDouble() * 2 - 1);
            wide[i] = i % 3 == 0 ? (r.nextDouble() - 0.5) * 20 : (r.nextDouble() - 0.5) * Math.pow(10, r.nextInt(30) - 10);
            pows[i] = r.nextDouble();
        }
        unary("strict_sin", angles, StrictMath::sin);
        unary("strict_cos", angles, StrictMath::cos);
        unary("strict_tan", small, StrictMath::tan);
        unary("strict_asin", unit, StrictMath::asin);
        unary("strict_atan", wide, StrictMath::atan);
        unary("strict_pow84", pows, x -> StrictMath.pow(x, 1.0 / 84));
        double[] big = new double[n];
        for (int i = 0; i < n; i++) big[i] = (r.nextDouble() - 0.5) * Math.pow(10, r.nextInt(12) + 3);
        unary("fast_sin", angles, FastMath::sin);
        unary("fast_cos", angles, FastMath::cos);
        unary("fast_sin_big", big, FastMath::sin);
        unary("fast_tan", angles, FastMath::tan);
        unary("fast_asin", unit, FastMath::asin);
        unary("fast_acos", unit, FastMath::acos);
        unary("fast_atan", wide, FastMath::atan);
        out.append(" \"fast_atan2\": [");
        for (int i = 0; i < n; i++) {
            double y = wide[i], x = wide[(i * 7 + 3) % n];
            if (i % 50 == 0) x = 0.0;
            if (i % 61 == 0) y = -0.0;
            out.append(i == 0 ? "" : ",").append("[").append(hex(y)).append(",").append(hex(x)).append(",").append(hex(FastMath.atan2(y, x))).append("]");
        }
        out.append("]\n}\n");
        System.out.print(out);
    }
}
