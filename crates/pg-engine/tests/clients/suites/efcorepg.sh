# EF Core 10 with the Npgsql provider, efcore.pg 10.0.3 (.NET): test/EFCore.PG.FunctionalTests
# (xUnit; the specification tests: Northwind queries, migrations, scaffolding / reverse
# engineering, type mappings, bulk updates, ...), as user npgsql_tests (a superuser, the way its
# CI makes it), which creates and drops its own databases. No PostGIS in either server, so the
# spatial tests skip themselves. Extra arguments go to `dotnet test` (`--filter ...`). About 7
# minutes on PostgreSQL, 32 on the engine.
# image: mcr.microsoft.com/dotnet/sdk:10.0
# src: https://github.com/npgsql/efcore.pg/archive/refs/tags/v10.0.3.tar.gz
set -eu
export DOTNET_CLI_HOME=/cache/home/dotnet NUGET_PACKAGES=/cache/home/nuget DOTNET_NOLOGO=1 \
  DOTNET_CLI_TELEMETRY_OPTOUT=1
export Test__Npgsql__DefaultConnection="Host=$PGHOST;Port=$PGPORT;Username=npgsql_tests;Password=npgsql_tests;SSL Mode=Disable;Timeout=60;Command Timeout=600"
cd test/EFCore.PG.FunctionalTests
set +e
timeout 7000 dotnet test -c Release -p:NuGetAudit=false -p:TreatWarningsAsErrors=false \
  --logger "trx;LogFileName=$OUT/efcorepg.trx" --results-directory "$OUT" \
  "$@" > "$OUT/dotnet.log" 2>&1
set -e
grep -E '^(Passed!|Failed!|Total|  Failed|Test Run)' "$OUT/dotnet.log" | tail -20
tail -5 "$OUT/dotnet.log"
