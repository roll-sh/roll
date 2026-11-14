import org.apache.lucene.util.IntroSorter;
import java.util.*;

/**
 * Reference permutations for `tests/sorters.rs`: Lucene's IntroSorter over arrays full of ties
 * (where an element ends up among equals is what the test pins down).
 * java -cp lucene-core-10.5.1.jar Sorters.java > ../aggs-sorters.json
 */
public class Sorters {
    public static void main(String[] args) {
        Random r = new Random(77);
        StringBuilder out = new StringBuilder("[\n");
        int cases = 160;
        for (int c = 0; c < cases; c++) {
            int n = c < 40 ? c : r.nextInt(c % 3 == 0 ? 1200 : 300);
            int distinct = 1 + r.nextInt(c % 2 == 0 ? 5 : 1000);
            final double[] v = new double[n];
            final int[] tag = new int[n];
            for (int i = 0; i < n; i++) {
                v[i] = r.nextInt(distinct);
                tag[i] = i;
            }
            // Some inputs presorted or reversed (they take other quicksort paths).
            if (c % 7 == 1) Arrays.sort(v);
            if (c % 7 == 2) { Arrays.sort(v); for (int i = 0; i < n / 2; i++) { double t = v[i]; v[i] = v[n - 1 - i]; v[n - 1 - i] = t; } }
            double[] in = v.clone();
            new IntroSorter() {
                double pivot;
                @Override protected void swap(int i, int j) { double t = v[i]; v[i] = v[j]; v[j] = t; int x = tag[i]; tag[i] = tag[j]; tag[j] = x; }
                @Override protected void setPivot(int i) { pivot = v[i]; }
                @Override protected int comparePivot(int j) { return Double.compare(pivot, v[j]); }
            }.sort(0, n);
            out.append(c == 0 ? "" : ",\n").append(" {\"values\": ").append(Arrays.toString(in)).append(", \"order\": ").append(Arrays.toString(tag)).append("}");
        }
        out.append("\n]\n");
        System.out.print(out);
    }
}
