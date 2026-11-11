CREATE FUNCTION gx_li_leak(integer,integer) RETURNS boolean AS 'int4lt' LANGUAGE internal IMMUTABLE STRICT;
SELECT gx_li_leak(1, 2), gx_li_leak(3, 2), gx_li_leak(NULL, 1);
CREATE TABLE gx_li_lt (a int, b int);
INSERT INTO gx_li_lt VALUES (1, 2), (5, 3);
SELECT * FROM gx_li_lt WHERE gx_li_leak(a, b);
CREATE FUNCTION gx_li_bad(int) RETURNS int AS 'no_such_fn' LANGUAGE internal;
DROP TABLE gx_li_lt; DROP FUNCTION gx_li_leak(integer, integer);
