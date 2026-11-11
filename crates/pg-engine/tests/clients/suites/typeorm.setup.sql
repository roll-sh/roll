-- TypeORM's CI PostgreSQL service: superuser "username" / "password" (POSTGRES_USER), database
-- typeorm. Its image also has PostGIS and pgvector; these servers don't, so the tests that use
-- them fail on both.
DROP DATABASE IF EXISTS typeorm;
DROP ROLE IF EXISTS username;
CREATE ROLE username SUPERUSER LOGIN PASSWORD 'password';
CREATE DATABASE typeorm OWNER username;
\c typeorm
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";
CREATE EXTENSION IF NOT EXISTS hstore;
