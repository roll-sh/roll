-- Knex's docker-compose PostgreSQL service: POSTGRES_USER testuser / knextest (a superuser),
-- database knex_test.
DROP DATABASE IF EXISTS knex_test;
DROP ROLE IF EXISTS testuser;
CREATE ROLE testuser SUPERUSER LOGIN PASSWORD 'knextest';
CREATE DATABASE knex_test OWNER testuser;
