-- r2dbc-postgresql's test server (its Testcontainers PostgreSQLContainer and
-- test-db-init-script.sql): superuser "test" owning database "test", and the login roles its
-- authentication tests use.
DROP DATABASE IF EXISTS test;
DROP ROLE IF EXISTS test;
DROP ROLE IF EXISTS "test-scram";
DROP ROLE IF EXISTS "test-ssl";
DROP ROLE IF EXISTS "test-ssl-with-cert";
CREATE ROLE test SUPERUSER LOGIN PASSWORD 'test';
CREATE DATABASE test OWNER test;
SET password_encryption = 'scram-sha-256';
CREATE ROLE "test-scram" LOGIN PASSWORD 'test-scram';
GRANT ALL PRIVILEGES ON DATABASE test TO "test-scram";
CREATE ROLE "test-ssl" LOGIN PASSWORD 'test-ssl';
GRANT ALL PRIVILEGES ON DATABASE test TO "test-ssl";
CREATE ROLE "test-ssl-with-cert" LOGIN;
GRANT ALL PRIVILEGES ON DATABASE test TO "test-ssl-with-cert";
