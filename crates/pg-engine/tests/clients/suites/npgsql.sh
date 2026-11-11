# Npgsql 9.0.3 (.NET): test/Npgsql.Tests (NUnit), against database npgsql_tests owned by
# user npgsql_tests, the way its CI sets them up.
# image: mcr.microsoft.com/dotnet/sdk:9.0
# src: https://github.com/npgsql/npgsql/archive/refs/tags/v9.0.3.tar.gz
set -eu
export DOTNET_CLI_HOME=/cache/home/dotnet NUGET_PACKAGES=/cache/home/nuget DOTNET_NOLOGO=1 \
  DOTNET_CLI_TELEMETRY_OPTOUT=1
export NPGSQL_TEST_DB="Host=$PGHOST;Port=$PGPORT;Username=npgsql_tests;Password=npgsql_tests;Database=npgsql_tests;Timeout=0;Command Timeout=0;SSL Mode=Disable"
cd test/Npgsql.Tests
set +e
timeout 5400 dotnet test -c Release -f net9.0 -p:NuGetAudit=false -p:TreatWarningsAsErrors=false --logger "trx;LogFileName=$OUT/npgsql.trx" \
  --filter "${NPGSQL_FILTER:-FullyQualifiedName!~Replication&FullyQualifiedName!~Kerberos}" \
  -- NUnit.NumberOfTestWorkers=1 "$@" > "$OUT/dotnet.log" 2>&1
set -e
tail -30 "$OUT/dotnet.log"
