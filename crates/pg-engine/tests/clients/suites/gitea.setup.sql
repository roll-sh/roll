-- Gitea's own database: its xorm migrations and schema sync make ~120 tables at startup.
DROP DATABASE IF EXISTS gitea_app;
CREATE DATABASE gitea_app;
