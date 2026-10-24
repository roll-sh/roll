# The Java driver suite. DRIVER is the Maven profile: datastax (org.apache.cassandra:java-driver-core)
# or scylla (com.scylladb:java-driver-core).
FROM maven:3-eclipse-temurin-21
ARG DRIVER=datastax
WORKDIR /suite
COPY pom.xml .
RUN mvn -q -B -P"$DRIVER" dependency:go-offline
COPY src src
RUN mvn -q -B -o -P"$DRIVER" package -DskipTests
ENTRYPOINT ["java", "-XX:+UseSerialGC", "-Xmx256m", "-cp", "/suite/target/classes:/suite/target/lib/*", "Suite"]
