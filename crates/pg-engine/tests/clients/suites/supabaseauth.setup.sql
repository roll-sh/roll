-- Supabase Auth's database (auth_app) as Supabase sets it up (hack/init_postgres.sql): its
-- admin role owns the auth schema its 75 migrations fill.
DROP DATABASE IF EXISTS auth_app;
DROP ROLE IF EXISTS supabase_auth_admin;
DROP ROLE IF EXISTS supabase_admin;
CREATE USER supabase_admin LOGIN CREATEROLE CREATEDB REPLICATION BYPASSRLS;
CREATE USER supabase_auth_admin NOINHERIT CREATEROLE LOGIN NOREPLICATION PASSWORD 'pw';
CREATE DATABASE auth_app;
\c auth_app
CREATE SCHEMA IF NOT EXISTS auth AUTHORIZATION supabase_auth_admin;
GRANT CREATE ON DATABASE auth_app TO supabase_auth_admin;
ALTER USER supabase_auth_admin SET search_path = 'auth';
