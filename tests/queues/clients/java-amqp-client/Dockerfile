# syntax=docker/dockerfile:1
FROM maven:3-eclipse-temurin-21 AS build
WORKDIR /src
COPY pom.xml .
RUN --mount=type=cache,target=/root/.m2 mvn -B -q dependency:resolve dependency:resolve-plugins
COPY src src
RUN --mount=type=cache,target=/root/.m2 mvn -B -q package dependency:copy-dependencies -DincludeScope=runtime -DoutputDirectory=target/lib

FROM eclipse-temurin:21-jre
WORKDIR /app
COPY --from=build /src/target/lib lib
COPY --from=build /src/target/app.jar app.jar
ENTRYPOINT ["java", "-cp", "app.jar:lib/*", "roll.qtest.Main"]
