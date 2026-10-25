-- README.unittests: the database and schemas (hstore / citext only where available).
DROP DATABASE IF EXISTS a38_sqla;
CREATE DATABASE a38_sqla ENCODING 'utf8' TEMPLATE template0;
\c a38_sqla
CREATE SCHEMA test_schema;
CREATE SCHEMA test_schema_2;
CREATE EXTENSION IF NOT EXISTS hstore;
CREATE EXTENSION IF NOT EXISTS citext;
