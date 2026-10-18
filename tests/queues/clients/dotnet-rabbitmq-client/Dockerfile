# syntax=docker/dockerfile:1
FROM mcr.microsoft.com/dotnet/sdk:9.0 AS build
WORKDIR /src
COPY DotnetRabbitmqClient.csproj .
RUN --mount=type=cache,target=/root/.nuget/packages dotnet restore
COPY Program.cs .
RUN --mount=type=cache,target=/root/.nuget/packages dotnet publish -c Release --no-restore -o /out

FROM mcr.microsoft.com/dotnet/runtime:9.0
WORKDIR /app
COPY --from=build /out .
ENTRYPOINT ["dotnet", "main.dll"]
