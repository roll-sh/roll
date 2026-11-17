#!/bin/sh
# Runs a dotnet command in the .NET 8 SDK image (this machine has no .NET): ./dotnet-in-docker.sh test
# As a non-root user here, .NET can't create a named mutex, which the CLI's first run and NuGet's
# one-time migrations take: their markers are made first, so neither runs.
# ROLL_DOTNET_NAME names the container, so it can be removed by name if it is ever left behind.
set -e
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$HOME/.cache/roll-secrets-nuget"
exec sg docker -c "docker run --rm ${ROLL_DOTNET_NAME:+--name $ROLL_DOTNET_NAME} --user $(id -u):$(id -g) \
  -e HOME=/tmp -e DOTNET_CLI_HOME=/tmp -e DOTNET_CLI_TELEMETRY_OPTOUT=1 -e DOTNET_NOLOGO=1 -e NUGET_PACKAGES=/nuget \
  -v $here:/src -v $HOME/.cache/roll-secrets-nuget:/nuget -w /src mcr.microsoft.com/dotnet/sdk:8.0 \
  sh -c 'v=\$(dotnet --version) && mkdir -p /tmp/.dotnet && for s in dotnetFirstUseSentinel toolpath.sentinel aspNetCertificateSentinel; do touch /tmp/.dotnet/\$v.\$s; done && mkdir -p /tmp/.local/share/NuGet/Migrations && touch /tmp/.local/share/NuGet/Migrations/1 && dotnet $*'"
