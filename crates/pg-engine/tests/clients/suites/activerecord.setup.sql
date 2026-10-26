-- Active Record's test databases (its Rakefile's db:postgresql:build makes them with createdb -E UTF8
-- -T template0; the C collation of the servers here instead of en_US.UTF-8).
DROP DATABASE IF EXISTS activerecord_unittest;
DROP DATABASE IF EXISTS activerecord_unittest2;
CREATE DATABASE activerecord_unittest ENCODING 'UTF8' TEMPLATE template0;
CREATE DATABASE activerecord_unittest2 ENCODING 'UTF8' TEMPLATE template0;
