-- SQLAlchemy's README.unittests: the database and its two test schemas.
DROP DATABASE IF EXISTS a81_alembic;
CREATE DATABASE a81_alembic ENCODING 'utf8' TEMPLATE template0;
\c a81_alembic
CREATE SCHEMA test_schema;
CREATE SCHEMA test_schema_2;
