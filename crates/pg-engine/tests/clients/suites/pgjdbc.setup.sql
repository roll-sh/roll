-- docker/postgres-server's setup: user and database "test" (and test_2), with hstore.
DROP DATABASE IF EXISTS test;
DROP DATABASE IF EXISTS test_2;
DROP ROLE IF EXISTS test;
CREATE USER test WITH PASSWORD 'test';
CREATE DATABASE test OWNER test;
CREATE DATABASE test_2 OWNER test;
\c test
CREATE EXTENSION IF NOT EXISTS hstore;
