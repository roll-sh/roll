-- ORDER BY ... USING a made operator: the order of its btree family's comparison function.
CREATE FUNCTION oou_rcmp(int4, int4) RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT CASE WHEN $1 = $2 THEN 0 WHEN $1 > $2 THEN -1 ELSE 1 END $$;
CREATE FUNCTION oou_rlt(int4, int4) RETURNS bool LANGUAGE sql IMMUTABLE AS $$ SELECT $1 > $2 $$;
CREATE FUNCTION oou_rle(int4, int4) RETURNS bool LANGUAGE sql IMMUTABLE AS $$ SELECT $1 >= $2 $$;
CREATE FUNCTION oou_rgt(int4, int4) RETURNS bool LANGUAGE sql IMMUTABLE AS $$ SELECT $1 < $2 $$;
CREATE FUNCTION oou_rge(int4, int4) RETURNS bool LANGUAGE sql IMMUTABLE AS $$ SELECT $1 <= $2 $$;
CREATE OPERATOR <<< (leftarg = int4, rightarg = int4, function = oou_rlt);
CREATE OPERATOR <<<= (leftarg = int4, rightarg = int4, function = oou_rle);
CREATE OPERATOR >>> (leftarg = int4, rightarg = int4, function = oou_rgt);
CREATE OPERATOR >>>= (leftarg = int4, rightarg = int4, function = oou_rge);
CREATE OPERATOR === (leftarg = int4, rightarg = int4, function = int4eq);
CREATE OPERATOR ~~~ (leftarg = int4, rightarg = int4, function = int4eq);
SELECT a FROM (VALUES (3), (1), (2)) v(a) ORDER BY a USING <<<;
CREATE OPERATOR CLASS oou_rev_ops FOR TYPE int4 USING btree AS
  OPERATOR 1 <<<, OPERATOR 2 <<<=, OPERATOR 3 ===, OPERATOR 4 >>>=, OPERATOR 5 >>>,
  FUNCTION 1 oou_rcmp(int4, int4);
CREATE TABLE oou_t (a int, b text);
INSERT INTO oou_t VALUES (3, 'c'), (1, 'a'), (NULL, 'n'), (2, 'b'), (5, 'e'), (4, 'd');
SELECT a FROM oou_t ORDER BY a USING <<<;
SELECT a FROM oou_t ORDER BY a USING >>>;
SELECT a FROM oou_t ORDER BY a USING <<< NULLS FIRST;
SELECT a, b FROM oou_t ORDER BY a USING >>> NULLS LAST, b;
SELECT a FROM oou_t ORDER BY a USING ~~~;
SELECT string_agg(a::text, ',' ORDER BY a USING <<<) FROM oou_t;
SELECT a, row_number() OVER (ORDER BY a USING <<<) FROM oou_t ORDER BY a;
