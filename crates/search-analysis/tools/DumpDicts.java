// Dumps what roll search's CJK analysis needs from Lucene's analysis-kuromoji, -nori and
// -smartcn jars (Apache-2.0; dictionaries IPADIC, mecab-ko-dic, ICTCLAS-derived): the
// TokenInfoDictionary FSTs as (surface, output) pairs, and smartcn's deserialized tables.
// Run with Lucene 10.5.1's core and analysis jars on the classpath: DumpDicts <outdir>
// (tools/cjk_dicts.sh).
import java.io.*;
import java.lang.reflect.Field;
import java.nio.charset.StandardCharsets;
import org.apache.lucene.store.*;
import org.apache.lucene.util.fst.*;
import org.apache.lucene.util.IntsRef;

public class DumpDicts {
  public static void main(String[] args) throws Exception {
    File out = new File(args[0]);
    fst("/org/apache/lucene/analysis/ja/dict/TokenInfoDictionary$fst.dat", new File(out, "ja-fst.bin"));
    fst("/org/apache/lucene/analysis/ko/dict/TokenInfoDictionary$fst.dat", new File(out, "ko-fst.bin"));
    smartcn(out);
  }

  static void writeVInt(DataOutputStream o, long v) throws IOException {
    while ((v & ~0x7FL) != 0) { o.write((int) ((v & 0x7F) | 0x80)); v >>>= 7; }
    o.write((int) v);
  }

  static void fst(String resource, File dst) throws Exception {
    try (InputStream is = new BufferedInputStream(DumpDicts.class.getResourceAsStream(resource));
         DataOutputStream o = new DataOutputStream(new BufferedOutputStream(new FileOutputStream(dst)))) {
      org.apache.lucene.store.DataInput in = new InputStreamDataInput(is);
      FST<Long> fst = new FST<>(FST.readMetadata(in, PositiveIntOutputs.getSingleton()), in);
      IntsRefFSTEnum<Long> e = new IntsRefFSTEnum<>(fst);
      IntsRefFSTEnum.InputOutput<Long> io;
      long n = 0;
      while ((io = e.next()) != null) {
        IntsRef k = io.input;
        char[] cs = new char[k.length];
        for (int i = 0; i < k.length; i++) cs[i] = (char) k.ints[k.offset + i];
        byte[] b = new String(cs).getBytes(StandardCharsets.UTF_8);
        writeVInt(o, b.length);
        o.write(b);
        writeVInt(o, io.output);
        n++;
      }
      System.err.println(resource + ": " + n + " entries");
    }
  }

  static Object field(Object o, String name) throws Exception {
    Field f = o.getClass().getDeclaredField(name);
    f.setAccessible(true);
    return f.get(o);
  }

  static Object instance(String cls) throws Exception {
    Class<?> c = Class.forName(cls);
    java.lang.reflect.Method m = c.getDeclaredMethod("getInstance");
    m.setAccessible(true);
    return m.invoke(null);
  }

  // smartcn.bin: words then bigrams, little-endian.
  // words: n chars; per char: u16 char, i32 table id. m tables; per table: i32 id, i32 count,
  //        per item: u16 len, len u16 chars, i32 freq.
  // bigrams: n; per entry (sorted by hash): i64 hash, i32 freq.
  static void smartcn(File out) throws Exception {
    Object wd = instance("org.apache.lucene.analysis.cn.smart.hhmm.WordDictionary");
    short[] wordIndexTable = (short[]) field(wd, "wordIndexTable");
    char[] charIndexTable = (char[]) field(wd, "charIndexTable");
    char[][][] items = (char[][][]) field(wd, "wordItem_charArrayTable");
    int[][] freqs = (int[][]) field(wd, "wordItem_frequencyTable");
    Object bd = instance("org.apache.lucene.analysis.cn.smart.hhmm.BigramDictionary");
    long[] hashes = (long[]) field(bd, "bigramHashTable");
    int[] bfreq = (int[]) field(bd, "frequencyTable");
    try (DataOutputStream o = new DataOutputStream(new BufferedOutputStream(new FileOutputStream(new File(out, "smartcn.bin"))))) {
      int nc = 0;
      for (int i = 0; i < charIndexTable.length; i++) if (charIndexTable[i] != 0) nc++;
      o.writeInt(Integer.reverseBytes(nc));
      for (int i = 0; i < charIndexTable.length; i++) {
        if (charIndexTable[i] == 0) continue;
        o.writeShort(Short.reverseBytes((short) charIndexTable[i]));
        o.writeInt(Integer.reverseBytes(wordIndexTable[i]));
      }
      int nt = 0;
      for (char[][] t : items) if (t != null) nt++;
      o.writeInt(Integer.reverseBytes(nt));
      int nullItems = 0;
      for (int id = 0; id < items.length; id++) {
        if (items[id] == null) continue;
        o.writeInt(Integer.reverseBytes(id));
        o.writeInt(Integer.reverseBytes(items[id].length));
        for (int j = 0; j < items[id].length; j++) {
          char[] w = items[id][j];
          if (w == null) { nullItems++; o.writeShort(Short.reverseBytes((short) 0xFFFF)); }
          else {
            o.writeShort(Short.reverseBytes((short) w.length));
            for (char c : w) o.writeShort(Short.reverseBytes((short) c));
          }
          o.writeInt(Integer.reverseBytes(freqs[id][j]));
        }
      }
      java.util.TreeMap<Long, Integer> m = new java.util.TreeMap<>();
      for (int i = 0; i < hashes.length; i++) {
        if (hashes[i] == 0) continue;
        if (m.put(hashes[i], bfreq[i]) != null) throw new IllegalStateException("dup hash");
      }
      int zeroSlotsWithFreq = 0;
      for (int i = 0; i < hashes.length; i++) if (hashes[i] == 0 && bfreq[i] != 0) zeroSlotsWithFreq++;
      o.writeInt(Integer.reverseBytes(m.size()));
      for (var e : m.entrySet()) {
        o.writeLong(Long.reverseBytes(e.getKey()));
        o.writeInt(Integer.reverseBytes(e.getValue()));
      }
      System.err.println("smartcn: " + nc + " chars, " + nt + " tables, " + nullItems + " null items, " + m.size() + " bigrams, " + zeroSlotsWithFreq + " zero-hash slots with freq");
    }
  }
}
