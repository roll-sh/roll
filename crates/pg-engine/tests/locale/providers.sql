-- Each provider's database default: case mapping, ordering (with and without an index),
-- regular expression classes, ILIKE, jsonb ordering and jsonpath's like_regex, text
-- search, the LIKE prefix as an index range (only in byte order), pg_collation's
-- "default" row and its actual version.
CREATE DATABASE locale_libc LOCALE 'en_US.UTF-8' TEMPLATE template0;
CREATE DATABASE locale_libc_ctype LC_CTYPE 'de_DE.UTF-8' TEMPLATE template0;
CREATE DATABASE locale_libc_cutf8 LOCALE 'C.UTF-8' TEMPLATE template0;
CREATE DATABASE locale_builtin LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' TEMPLATE template0;
CREATE DATABASE locale_fast LOCALE_PROVIDER builtin BUILTIN_LOCALE 'PG_UNICODE_FAST' TEMPLATE template0;
CREATE DATABASE locale_icu LOCALE_PROVIDER icu ICU_LOCALE 'en-US' TEMPLATE template0;
CREATE DATABASE locale_icu_tr LOCALE_PROVIDER icu LOCALE 'tr-TR' TEMPLATE template0;
CREATE DATABASE locale_icu_sv LOCALE_PROVIDER icu ICU_LOCALE 'sv-SE' TEMPLATE template0;
CREATE DATABASE locale_icu_rules LOCALE_PROVIDER icu ICU_LOCALE 'de' ICU_RULES '&a < b' TEMPLATE template0;
CREATE DATABASE locale_icu_ks LOCALE_PROVIDER icu ICU_LOCALE 'en-u-ks-level1' TEMPLATE template0;
\c postgres
\i battery
\c locale_libc
\i battery
\c locale_libc_ctype
\i battery
\c locale_libc_cutf8
\i battery
\c locale_builtin
\i battery
\c locale_fast
\i battery
\c locale_icu
\i battery
\c locale_icu_tr
\i battery
\c locale_icu_sv
\i battery
\c locale_icu_rules
\i battery
\c locale_icu_ks
\i battery
