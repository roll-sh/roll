-- A one-value IN is `x = v` (NOT IN: `x <> v`) as make_op resolves it: over an array of a
-- domain against the base type's array it finds no operator, as `=` doesn't (agent 84's open
-- item, seed 4012's kin).
CREATE DOMAIN a104_dint AS int CHECK (VALUE > -10);
CREATE TABLE a104_is (id int, d a104_dint, t text, n numeric);
INSERT INTO a104_is VALUES (1, 1, 'a', 1.5), (2, NULL, NULL, NULL), (3, 3, 'b', 2);
SELECT id FROM a104_is WHERE ARRAY[d] IN ('{1}'::int4[]);
SELECT id FROM a104_is WHERE ARRAY[d] = '{1}'::int4[];
SELECT id FROM a104_is WHERE ARRAY[d] IN ('{1}'::int4[], '{3}'::int4[]);
SELECT id FROM a104_is WHERE d IN (1) ORDER BY id;
SELECT id FROM a104_is WHERE d NOT IN (1) ORDER BY id;
SELECT id FROM a104_is WHERE t IN ('a') ORDER BY id;
SELECT id FROM a104_is WHERE n IN (2) ORDER BY id;
SELECT id FROM a104_is WHERE NULL IN (1) ORDER BY id;
SELECT id, id IN (n), t NOT IN ('b') FROM a104_is ORDER BY id;
SELECT id FROM a104_is WHERE t IN (1);
CREATE VIEW a104_isv AS SELECT id FROM a104_is WHERE d IN (1) AND t NOT IN ('x');
SELECT pg_get_viewdef('a104_isv'::regclass);
DROP VIEW a104_isv;
DROP TABLE a104_is;
DROP DOMAIN a104_dint;
