-- tests/bootstrap.js's roles and database, with a password for the plain test role.
DROP DATABASE IF EXISTS postgres_js_test;
DROP ROLE IF EXISTS postgres_js_test, postgres_js_test_md5, postgres_js_test_scram;
CREATE USER postgres_js_test PASSWORD 'postgres_js_test';
SET password_encryption = md5;
CREATE USER postgres_js_test_md5 PASSWORD 'postgres_js_test_md5';
SET password_encryption = 'scram-sha-256';
CREATE USER postgres_js_test_scram PASSWORD 'postgres_js_test_scram';
CREATE DATABASE postgres_js_test OWNER postgres_js_test;
GRANT ALL ON DATABASE postgres_js_test TO postgres_js_test;
