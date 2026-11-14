#!/usr/bin/env bash
# Compares search-index query latency with Lucene on the same corpus and queries.
#   crates/search-index/benches/lucene/run.sh [work dir]
# Exports the corpus from `tests/bench.rs` (`bench_vs_lucene`, release), then runs
# LuceneBench.java on Lucene 10.5.1 in a JDK 21 container (Maven Central jars, cached in the
# work dir). Set BENCH_BP=0 to measure roll without BP reordering.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
work="${1:-/tmp/roll-search-lucene-bench}"
lucene=10.5.1
image="${LUCENE_BENCH_IMAGE:-eclipse-temurin:21-jdk}"
mkdir -p "$work/jars"
for a in lucene-core lucene-analysis-common; do
  jar="$work/jars/$a-$lucene.jar"
  [ -f "$jar" ] || curl -sfo "$jar" "https://repo1.maven.org/maven2/org/apache/lucene/$a/$lucene/$a-$lucene.jar"
done
BENCH_EXPORT="$work" cargo test -p search-index --release --test bench -- --ignored --nocapture bench_vs_lucene
cp "$here/LuceneBench.java" "$work/"
docker run --rm -m 3g -e BENCH_ROUNDS -e BENCH_ONLY -v "$work:/w" -w /w --entrypoint sh "$image" -c \
  'javac -cp "jars/*" -d classes LuceneBench.java && java --add-modules jdk.incubator.vector --enable-native-access=ALL-UNNAMED -Xms2g -Xmx2g -cp "classes:jars/*" LuceneBench /w'
