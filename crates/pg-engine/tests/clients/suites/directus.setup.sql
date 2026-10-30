-- Directus's database (directus_app): its migrations make the directus_* tables at
-- bootstrap; the collections the suite creates go into it too.
DROP DATABASE IF EXISTS directus_app;
CREATE DATABASE directus_app;
