FROM maven:3-eclipse-temurin-21
WORKDIR /tc
COPY pom.xml .
RUN mvn -q -B dependency:go-offline
COPY src src
RUN mvn -q -B -o package -DskipTests
ENTRYPOINT ["java", "-XX:+UseSerialGC", "-Xmx256m", "-Dorg.slf4j.simpleLogger.defaultLogLevel=warn", "-cp", "/tc/target/classes:/tc/target/lib/*", "TcCheck"]
