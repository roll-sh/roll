-- Kysely's docker-compose PostgreSQL service: POSTGRES_USER kysely / kysely (a superuser),
-- database kysely_test.
DROP DATABASE IF EXISTS kysely_test;
DROP ROLE IF EXISTS kysely;
CREATE ROLE kysely SUPERUSER LOGIN PASSWORD 'kysely';
CREATE DATABASE kysely_test OWNER kysely;
