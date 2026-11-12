# The Java driver suite. DRIVER is the Maven profile: datastax (org.apache.cassandra:java-driver-core)
# or scylla (com.scylladb:java-driver-core).
FROM maven:3-eclipse-temurin-21@sha256:9673eef9ed503146591667ffb92eaaa22f1b8899ed60d9df0df4da3f3a1214a6
ARG DRIVER=datastax
WORKDIR /suite
COPY pom.xml .
RUN mvn -q -B -P"$DRIVER" dependency:go-offline
COPY src src
RUN mvn -q -B -o -P"$DRIVER" package -DskipTests
ENTRYPOINT ["java", "-XX:+UseSerialGC", "-Xmx256m", "-cp", "/suite/target/classes:/suite/target/lib/*", "Suite"]
