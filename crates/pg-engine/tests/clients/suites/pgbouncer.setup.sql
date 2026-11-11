-- pgbouncer's databases and roles (pgbouncer suite): pgb_test, owned by pgb_user (a SCRAM
-- password pgbouncer's auth_query reads from pg_shadow as postgres).
DROP DATABASE IF EXISTS pgb_test;
DROP ROLE IF EXISTS pgb_user;
CREATE ROLE pgb_user LOGIN PASSWORD 'pgbpw';
CREATE DATABASE pgb_test OWNER pgb_user TEMPLATE template0 ENCODING 'UTF8';
\c pgb_test
SET ROLE pgb_user;
CREATE TABLE items (id int PRIMARY KEY, name text NOT NULL UNIQUE, qty int NOT NULL DEFAULT 0, tags text[]);
INSERT INTO items SELECT g, 'item' || g, g * 10, ARRAY['t' || (g % 3)] FROM generate_series(1, 200) g;
CREATE TABLE counter (id int PRIMARY KEY, n int NOT NULL);
INSERT INTO counter VALUES (1, 0);
CREATE FUNCTION slow(s float8) RETURNS int LANGUAGE sql AS 'SELECT 1 FROM pg_sleep(s)';
