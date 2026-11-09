# syntax=docker/dockerfile:1
FROM maven:3-eclipse-temurin-21@sha256:9673eef9ed503146591667ffb92eaaa22f1b8899ed60d9df0df4da3f3a1214a6 AS build
WORKDIR /src
COPY pom.xml .
RUN --mount=type=cache,target=/root/.m2 mvn -B -q dependency:resolve dependency:resolve-plugins
COPY src src
RUN --mount=type=cache,target=/root/.m2 mvn -B -q package dependency:copy-dependencies -DincludeScope=runtime -DoutputDirectory=target/lib

FROM eclipse-temurin:21-jre@sha256:0b99b3376fbb50dbf9c0af8535fdd5b8eb0d3fdfc8b1e4967f383b6919f6901a
WORKDIR /app
COPY --from=build /src/target/lib lib
COPY --from=build /src/target/app.jar app.jar
ENTRYPOINT ["java", "-cp", "app.jar:lib/*", "roll.qtest.Main"]
