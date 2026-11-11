# Dapper 2.1.89 (.NET micro-ORM over Npgsql 10): tests/Dapper.Tests, net10.0. Its own
# PostgreSQL tests (Providers/PostgresqlTests.cs, PostgesConnectionString) plus its
# provider-generic test classes (MiscTests<TProvider>, ParameterTests<TProvider>, ...), which
# upstream instantiates only for SQL Server / SQLite, instantiated here for its PostgresProvider
# too (Npgsql* classes; not AsyncTests / AsyncQueryCacheTests, which require a SQL Server
# provider, nor SingleRowTests, which needs a provider-specific bulk loader). Many of those
# use T-SQL and fail on PostgreSQL as well; only differences between the two servers count. Database dappertest owned by dappertest, as its docs set up.
# image: mcr.microsoft.com/dotnet/sdk:10.0
# src: https://github.com/DapperLib/Dapper/archive/refs/tags/2.1.89.tar.gz
set -eu
export DOTNET_CLI_HOME=/cache/home/dotnet NUGET_PACKAGES=/cache/home/nuget DOTNET_NOLOGO=1 \
  DOTNET_CLI_TELEMETRY_OPTOUT=1
export PostgesConnectionString="Server=$PGHOST;Port=$PGPORT;User Id=dappertest;Password=dapperpass;Database=dappertest;SSL Mode=Disable"
W=/cache/work/dapper
mkdir -p $W
# Keep obj/ and bin/ between runs (restore and build are the slow part).
cp -r "$SRC/." $W/
T=$W/tests/Dapper.Tests
{
  echo 'namespace Dapper.Tests {'
  grep -h -o 'abstract class [A-Za-z]*<TProvider>' $T/*.cs | sed 's/abstract class //; s/<TProvider>//' |
    grep -v -x -e TestBase -e AsyncTests -e AsyncQueryCacheTests -e SingleRowTests | sort -u | while read -r c; do
      echo "  [Xunit.Collection(\"Npgsql\")] public sealed class Npgsql$c : $c<PostgresProvider> { }"
    done
  echo '}'
} > $T/NpgsqlGenerated.cs
cd $T
set +e
timeout 3000 dotnet test -c Release -f net10.0 -p:NuGetAudit=false -p:TreatWarningsAsErrors=false \
  -p:SignAssembly=false --logger "trx;LogFileName=$OUT/dapper.trx" --results-directory "$OUT" \
  --filter "FullyQualifiedName~Npgsql|FullyQualifiedName~PostgresqlTests" "$@" > "$OUT/dotnet.log" 2>&1
set -e
grep -E '^(Passed!|Failed!)|error CS' "$OUT/dotnet.log" | tail -20
