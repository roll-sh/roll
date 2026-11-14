#!/usr/bin/env bash
# Regenerates the dictionaries of the kuromoji, nori and smartcn components
# (src/kuromoji/ipadic.dict, src/nori/mecab-ko-dic.dict, src/smartcn/hhmm.dict) from Lucene
# 10.5.1's analysis jars (Apache-2.0; IPADIC, mecab-ko-dic and Lucene's own Chinese
# dictionaries). The jars come from Maven Central; DumpDicts.java runs on the JDK of the
# Elasticsearch image (with Lucene's core jar from it) and reads out what only Lucene can read
# (its FSTs, smartcn's serialized tables); examples/gen_cjk_dicts.rs packs everything.
#
# usage: crates/search-analysis/tools/cjk_dicts.sh [work dir]   (needs docker and cargo)
set -euo pipefail
cd "$(dirname "$0")/.."
CRATE=$PWD
WORK=${1:-$HOME/.cache/rsearch/cjk-dicts}
LUCENE=10.5.1
IMAGE=docker.elastic.co/elasticsearch/elasticsearch:9.5.5
mkdir -p "$WORK/jars" "$WORK/dump"
for a in kuromoji nori smartcn; do
  jar=lucene-analysis-$a-$LUCENE.jar
  [[ -f $WORK/jars/$jar ]] || curl -fsSL -o "$WORK/jars/$jar" \
    "https://repo1.maven.org/maven2/org/apache/lucene/lucene-analysis-$a/$LUCENE/$jar"
done
for l in ja:kuromoji ko:nori; do
  mkdir -p "$WORK/dump/${l%%:*}"
  unzip -qjo "$WORK/jars/lucene-analysis-${l##*:}-$LUCENE.jar" "org/apache/lucene/analysis/${l%%:*}/dict/*.dat" -d "$WORK/dump/${l%%:*}"
done
cp tools/DumpDicts.java "$WORK/"
docker run --rm -v "$WORK:/w" --entrypoint /usr/share/elasticsearch/jdk/bin/java "$IMAGE" \
  -cp "/usr/share/elasticsearch/lib/*:/w/jars/*" /w/DumpDicts.java /w/dump
cargo run --release -q -p search-analysis --example gen_cjk_dicts -- "$WORK/dump" "$CRATE/src"
