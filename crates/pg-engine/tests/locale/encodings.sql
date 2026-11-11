-- Databases in other encodings than UTF8: CREATE DATABASE's checks, server_encoding and
-- client_encoding, conversions between the client's encoding and the database's, and
-- what functions make of text in the database's encoding (its bytes).
CREATE DATABASE locale_sa ENCODING 'sql_ascii' LOCALE_PROVIDER libc TEMPLATE template0;
CREATE DATABASE locale_sa2 ENCODING 'SQL_ASCII' LOCALE 'en_US.UTF-8' TEMPLATE template0;
CREATE DATABASE locale_l1 ENCODING 'LATIN1' TEMPLATE template0;
CREATE DATABASE locale_w1 ENCODING 'WIN1251' LOCALE 'C' TEMPLATE template0;
CREATE DATABASE locale_ej ENCODING 'EUC_JP' TEMPLATE template0;
CREATE DATABASE locale_tw ENCODING 'EUC_TW' TEMPLATE template0;
CREATE DATABASE locale_mu ENCODING 'MULE_INTERNAL' TEMPLATE template0;
CREATE DATABASE locale_l9 ENCODING 'LATIN9' LOCALE_PROVIDER icu ICU_LOCALE 'de' TEMPLATE template0;
CREATE DATABASE locale_x1 ENCODING 'SJIS' TEMPLATE template0;
CREATE DATABASE locale_x2 ENCODING 'LATIN1' TEMPLATE template1;
CREATE DATABASE locale_x3 ENCODING 'KOI8R' LOCALE 'ru_RU.UTF-8' TEMPLATE template0;
CREATE DATABASE locale_x4 TEMPLATE locale_l1;
SELECT datname, pg_encoding_to_char(encoding), datcollate, datctype, datlocprovider, datlocale
FROM pg_database WHERE datname LIKE 'locale\_%' ORDER BY 1;
\c locale_l1
SHOW server_encoding;
SHOW client_encoding;
SELECT getdatabaseencoding(), pg_client_encoding();
SELECT character_set_name, character_repertoire, form_of_use FROM information_schema.character_sets;
CREATE TABLE t (a text);
INSERT INTO t VALUES ('é'), ('ÿ'), ('a'), ('Z'), ('ß'), ('Éa');
SELECT a, length(a), octet_length(a), bit_length(a), ascii(a), upper(a), lower(a), initcap(a), a::bytea AS b FROM t ORDER BY 1;
SELECT string_agg(a, ',' ORDER BY a COLLATE "C") FROM t;
SELECT a, a ~ '^[[:alpha:]]+$' AS alpha, a ILIKE 'é%' AS ilike, a::varchar(1) AS v FROM t ORDER BY 1;
INSERT INTO t VALUES ('€');
SELECT 'x€';
SELECT chr(233), chr(255), ascii(chr(200));
SELECT chr(256);
SELECT convert_to('é', 'UTF8'), convert_to('é', 'LATIN1'), convert_from('\xe9', 'LATIN1'), convert_from('\xc3a9', 'UTF8');
SELECT convert_from('\xe282ac', 'UTF8');
SELECT md5('é'), 'é'::"char", 'é'::"char"::text, octet_length('é'::char(3)), 'é'::bytea;
SELECT U&'\00e9', U&'\0041';
SELECT U&'\20ac';
SELECT E'\xe9', E'\351\n', E'\u00e9', E'a\\\xff', E'\x41';
SELECT E'\u20ac';
SELECT U&'d!00e9' UESCAPE '!';
SET standard_conforming_strings = off;
SELECT 'x\xe9', '\\\xe9';
RESET standard_conforming_strings;
SELECT '"\u00e9"'::jsonb, '"\u00e9"'::json, '["\u00e9"]'::json ->> 0, '["\u0041"]'::jsonb ->> 0;
SELECT '"\u20ac"'::jsonb;
SELECT unistr('\00e9x');
SELECT unistr('\20ac');
SELECT normalize('a');
SELECT 'a' IS NFC NORMALIZED;
SELECT casefold('A');
SELECT unicode_assigned('a');
SELECT to_ascii('éàç'), to_ascii('éàç', 'LATIN1');
SELECT length(repeat('é', 70)::name), octet_length(repeat('é', 70)::name);
SET client_encoding = 'LATIN2';
SET client_encoding = 'MULE_INTERNAL';
SELECT 'é' AS mule, pg_client_encoding();
RESET client_encoding;
SHOW client_encoding;
SET client_encoding = 'WIN1252';
SELECT 'é' AS win1252;
SET client_encoding = 'UTF8';
SELECT a, length(a) FROM t WHERE a = 'é';
\c locale_sa
SHOW server_encoding;
SELECT getdatabaseencoding(), 'é' AS e, length('é'), octet_length('é'), ascii('é'), upper('é'), 'é'::bytea;
SELECT chr(233) AS bad;
SELECT convert_to('é', 'LATIN1');
SELECT convert_from('\xe9', 'LATIN1');
SELECT U&'\00e9';
SELECT unistr('\00e9');
SELECT '"\u00e9"'::jsonb;
SELECT E'\xe9' AS bad;
SET client_encoding = 'LATIN1';
SELECT octet_length('é'), length('é');
SET client_encoding = 'SJIS';
SELECT 'x' AS sjis;
SET client_encoding = 'UTF8';
\c locale_sa2
SELECT upper('é'), lower('É'), 'é' ~ '[[:alpha:]]';
\c locale_w1
SHOW server_encoding;
SELECT x, length(x), octet_length(x), ascii(x), x::bytea AS b FROM (VALUES ('ж'), ('я'), ('Ё'), ('a'), ('€')) v(x) ORDER BY x;
SELECT 'ж' < 'я', '€' < 'ж';
\c locale_ej
SHOW server_encoding;
SELECT x, length(x), octet_length(x), x::bytea AS b FROM (VALUES ('日本'), ('ｱ'), ('a'), ('漢字'), ('丂')) v(x) ORDER BY x;
SELECT ascii('日');
SELECT chr(128);
SELECT substr('日本語', 2, 1), position('語' IN '日本語'), reverse('日本語'), left('日本語', 1);
SELECT convert_to('日本', 'UTF8'), convert_to('日本', 'EUC_JP'), convert_from('\xc6fc', 'EUC_JP');
SELECT '日本'::bytea, md5('日本');
SELECT '한';
SELECT E'\xc6\xfc', E'\xc6', U&'\65e5';
\c locale_tw
SELECT x, length(x), octet_length(x), x::bytea AS b FROM (VALUES ('中文'), ('a')) v(x) ORDER BY x;
\c locale_mu
\c locale_l9
SHOW server_encoding;
SELECT x, upper(x), lower(x) FROM (VALUES ('é'), ('z'), ('a'), ('€'), ('Ä')) v(x) ORDER BY x;
SELECT 'ä' < 'b', 'ä' < 'b' COLLATE "C";
\c postgres
