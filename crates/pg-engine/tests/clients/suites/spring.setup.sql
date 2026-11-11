-- The spring suite's databases: Spring JDBC's, Flyway's, and Liquibase's (its changelog target,
-- a reference built to tag v1 for diff, and the generated changelog's round-trip target).
DROP DATABASE IF EXISTS spring_jdbc;
DROP DATABASE IF EXISTS flyway_test;
DROP DATABASE IF EXISTS liquibase_test;
DROP DATABASE IF EXISTS liquibase_ref;
DROP DATABASE IF EXISTS liquibase_gen;
CREATE DATABASE spring_jdbc;
CREATE DATABASE flyway_test;
CREATE DATABASE liquibase_test;
CREATE DATABASE liquibase_ref;
CREATE DATABASE liquibase_gen;
