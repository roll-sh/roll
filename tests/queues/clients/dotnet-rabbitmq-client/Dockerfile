# syntax=docker/dockerfile:1
FROM mcr.microsoft.com/dotnet/sdk:9.0@sha256:07ed28d9e7279248fcea27334880f6c415b42c631c39b8bc5b3caf13ec6fdaa2 AS build
WORKDIR /src
COPY DotnetRabbitmqClient.csproj .
RUN --mount=type=cache,target=/root/.nuget/packages dotnet restore
COPY Program.cs .
RUN --mount=type=cache,target=/root/.nuget/packages dotnet publish -c Release --no-restore -o /out

FROM mcr.microsoft.com/dotnet/runtime:9.0@sha256:8e09d303d50457f544b0f14bc5f448cb0b218a7ba92d09cd3c079a382913f292
WORKDIR /app
COPY --from=build /out .
ENTRYPOINT ["dotnet", "main.dll"]
