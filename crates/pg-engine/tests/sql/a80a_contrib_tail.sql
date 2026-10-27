-- Contrib tail (agent 80a): intagg's int_array_aggregate (an aggregate over a built-in
-- aggregate's internal transition functions), spi's trigger functions (autoinc,
-- insert_username, moddatetime, refint), xml2's XPath functions and xpath_table,
-- tsm_system_time's Materialize.
CREATE EXTENSION intagg;
CREATE TABLE ia (g int, v int);
INSERT INTO ia VALUES (1, 3), (1, 1), (2, 5), (1, NULL), (2, 7);
SELECT g, int_array_aggregate(v) FROM ia GROUP BY g ORDER BY g;
SELECT int_array_aggregate(v ORDER BY v DESC NULLS LAST) FROM ia;
SELECT int_array_aggregate(v), int_array_aggregate(v) FILTER (WHERE g = 2) FROM ia;
SELECT int_array_aggregate(v) FROM ia WHERE false;
SELECT int_array_enum(int_array_aggregate(v)) FROM ia WHERE g = 2;
SELECT g, int_array_aggregate(v) OVER (PARTITION BY g) FROM ia ORDER BY 1, 2;
CREATE AGGREGATE my_text_agg(text, text) (SFUNC = string_agg_transfn, STYPE = internal, FINALFUNC = string_agg_finalfn);
SELECT my_text_agg(v::text, '-' ORDER BY v) FROM ia;
DROP AGGREGATE my_text_agg(text, text);
DROP TABLE ia;
DROP EXTENSION intagg;

CREATE EXTENSION autoinc;
CREATE EXTENSION insert_username;
CREATE EXTENSION moddatetime;
CREATE EXTENSION refint;
CREATE SEQUENCE ai_seq INCREMENT 10 START 0 MINVALUE 0;
CREATE TABLE ai (id int4, v text, who text, at timestamp, atz timestamptz);
CREATE TRIGGER ai_id BEFORE INSERT OR UPDATE ON ai FOR EACH ROW EXECUTE PROCEDURE autoinc(id, ai_seq);
CREATE TRIGGER ai_who BEFORE INSERT OR UPDATE ON ai FOR EACH ROW EXECUTE PROCEDURE insert_username(who);
CREATE TRIGGER ai_at BEFORE UPDATE ON ai FOR EACH ROW EXECUTE PROCEDURE moddatetime(at);
CREATE TRIGGER ai_atz BEFORE UPDATE ON ai FOR EACH ROW EXECUTE PROCEDURE moddatetime(atz);
INSERT INTO ai (id, v) VALUES (NULL, 'a'), (0, 'b'), (7, 'c');
SELECT id, v, who, at IS NULL, atz IS NULL FROM ai ORDER BY v;
BEGIN;
UPDATE ai SET id = 0 WHERE v = 'c';
SELECT id, v, at = localtimestamp, atz = now() FROM ai ORDER BY v;
COMMIT;
CREATE TRIGGER ai_bad BEFORE INSERT ON ai FOR EACH ROW EXECUTE PROCEDURE moddatetime(at);
INSERT INTO ai (v) VALUES ('x');
DROP TRIGGER ai_bad ON ai;
CREATE TRIGGER ai_bad BEFORE UPDATE ON ai FOR EACH ROW EXECUTE PROCEDURE moddatetime(v);
UPDATE ai SET v = v;
DROP TRIGGER ai_bad ON ai;
CREATE TRIGGER ai_bad BEFORE UPDATE ON ai FOR EACH ROW EXECUTE PROCEDURE autoinc(v, ai_seq);
UPDATE ai SET v = v;
DROP TRIGGER ai_bad ON ai;
CREATE TRIGGER ai_bad BEFORE UPDATE ON ai FOR EACH ROW EXECUTE PROCEDURE autoinc(nope, ai_seq);
UPDATE ai SET v = v;
DROP TRIGGER ai_bad ON ai;
CREATE TRIGGER ai_bad AFTER UPDATE ON ai FOR EACH ROW EXECUTE PROCEDURE insert_username(who);
UPDATE ai SET v = v;
DROP TRIGGER ai_bad ON ai;
CREATE TRIGGER ai_bad BEFORE DELETE ON ai FOR EACH ROW EXECUTE PROCEDURE autoinc(id, ai_seq);
DELETE FROM ai;
DROP TRIGGER ai_bad ON ai;
SELECT autoinc();
DROP TABLE ai;
DROP SEQUENCE ai_seq;

CREATE TABLE pk (a int NOT NULL, b text NOT NULL);
CREATE TABLE fk (x int, y text);
INSERT INTO pk VALUES (1, 'one'), (2, 'two'), (3, 'it''s');
CREATE TRIGGER fk_check AFTER INSERT OR UPDATE ON fk FOR EACH ROW
  EXECUTE FUNCTION check_primary_key('x', 'y', 'pk', 'a', 'b');
CREATE TRIGGER pk_cascade AFTER DELETE OR UPDATE ON pk FOR EACH ROW
  EXECUTE FUNCTION check_foreign_key(1, 'cascade', 'a', 'b', 'fk', 'x', 'y');
INSERT INTO fk VALUES (1, 'one'), (2, 'two'), (3, 'it''s'), (NULL, 'zzz');
INSERT INTO fk VALUES (4, 'four');
UPDATE pk SET b = 'uno' WHERE a = 1;
UPDATE pk SET a = 30 WHERE a = 3;
DELETE FROM pk WHERE a = 2;
UPDATE pk SET a = a;
SELECT * FROM fk ORDER BY x;
DROP TRIGGER pk_cascade ON pk;
CREATE TRIGGER pk_restrict AFTER DELETE ON pk FOR EACH ROW
  EXECUTE FUNCTION check_foreign_key(1, 'restrict', 'a', 'b', 'fk', 'x', 'y');
DELETE FROM pk WHERE a = 1;
DROP TRIGGER pk_restrict ON pk;
CREATE TRIGGER pk_setnull AFTER DELETE ON pk FOR EACH ROW
  EXECUTE FUNCTION check_foreign_key(1, 'setnull', 'a', 'b', 'fk', 'x', 'y');
DELETE FROM pk WHERE a = 1;
SELECT * FROM fk ORDER BY x;
DROP TRIGGER pk_setnull ON pk;
CREATE TRIGGER pk_bad AFTER DELETE ON pk FOR EACH ROW
  EXECUTE FUNCTION check_foreign_key(1, 'cascade', 'a', 'fk');
DELETE FROM pk;
DROP TRIGGER pk_bad ON pk;
CREATE TRIGGER pk_bad AFTER DELETE ON pk FOR EACH ROW
  EXECUTE FUNCTION check_foreign_key(1, 'bogus', 'a', 'b', 'fk', 'x', 'y');
DELETE FROM pk;
DROP TRIGGER pk_bad ON pk;
CREATE TRIGGER pk_bad AFTER DELETE ON pk FOR EACH ROW
  EXECUTE FUNCTION check_foreign_key(1, 'cascade', 'nope', 'fk', 'x');
DELETE FROM pk;
DROP TRIGGER pk_bad ON pk;
DROP TABLE pk, fk;
DROP EXTENSION autoinc;
DROP EXTENSION insert_username;
DROP EXTENSION moddatetime;
DROP EXTENSION refint;

CREATE EXTENSION xml2;
SELECT xpath_string('<a><b>x</b><b>y</b></a>', 'b'), xpath_string('<a><b>x</b></a>', 'count(b)');
SELECT xpath_string('<a><b>x</b>', '/a/b') IS NULL, xpath_string('<a/>', '$v') IS NULL;
SELECT xpath_string('<a/>', '/a/b[');
SELECT xpath_number('<a><b>3.5</b></a>', '/a/b'), xpath_number('<a><b>x</b></a>', '/a/b') IS NULL, xpath_number('<a/>', '1 div 3');
SELECT xpath_bool('<a><b>3.5</b></a>', '/a/b'), xpath_bool('<a/>', '/a/c'), xpath_bool('<a', '/a');
SELECT xpath_nodeset('<a><b c="1">x&amp;y</b><b>é</b></a>', '//b'), xpath_nodeset('<a><b c="é"/></a>', '//@c');
SELECT xpath_nodeset('<a><b>x</b><c/></a>', '/a/node()', 't', 's');
SELECT xpath_nodeset('<a><b>x</b></a>', '//b', 'item');
SELECT xpath_nodeset('<a><b>x</b></a>', 'count(//b)');
SELECT xpath_nodeset('<root xmlns:foo="http://icl.com/saxon"/>', '//namespace::foo');
SELECT xpath_nodeset('<!DOCTYPE a [<!ENTITY e "ent">]><a>&e;</a>', '/a');
SELECT xpath_list('<a><b>x</b><b>y</b></a>', '//b'), xpath_list('<a><b>x</b><b>y</b></a>', '//b', '|');
SELECT xml_encode_special_chars('<a href="x">&amp;</a>'), xml_valid('<a/>'), xml_valid('<a');
CREATE TABLE xt (id int, d text);
INSERT INTO xt VALUES (1, '<r><v>1</v><v>2</v><w>a</w></r>'), (2, 'bad'), (3, NULL), (4, '<r/>');
SELECT * FROM xpath_table('id', 'd', 'xt', '/r/v|/r/w|count(/r/v)', 'true') AS t(id int, v int, w text, c text) ORDER BY 1;
SELECT * FROM xpath_table('id', 'd', 'xt', '/r/v|/r/w', 'id < 4') AS t(id int, v int, w text, x text) ORDER BY 1;
SELECT * FROM xpath_table('id', 'd', 'xt', '/r/v', 'true') AS t(id text) ORDER BY 1;
SELECT * FROM xpath_table('id, id', 'd', 'xt', '/r/v', 'true') AS t(id int, v int);
SELECT * FROM xpath_table('id', 'd', 'xt', '/r/v[', 'true') AS t(id int, v int);
SELECT * FROM xpath_table('id', 'd', 'xt', '/r/v', 'nope') AS t(id int, v int);
DROP TABLE xt;
DROP EXTENSION xml2;

CREATE TABLE ts (id int, name text);
INSERT INTO ts SELECT i, repeat(i::text, 10) FROM generate_series(0, 30) s(i);
ANALYZE ts;
CREATE EXTENSION tsm_system_time;
EXPLAIN (COSTS OFF) SELECT * FROM (VALUES (0), (100000)) v(time),
  LATERAL (SELECT count(*) FROM ts TABLESAMPLE system_time (100000)) ss;
EXPLAIN (COSTS OFF) SELECT * FROM (VALUES (0), (100000)) v(time),
  LATERAL (SELECT count(*) FROM ts TABLESAMPLE system_time (time)) ss;
SELECT * FROM (VALUES (0), (100000)) v(time),
  LATERAL (SELECT count(*) FROM ts TABLESAMPLE system_time (100000)) ss;
EXPLAIN (COSTS OFF) SELECT * FROM ts TABLESAMPLE system_time (20) WHERE id > 3;
EXPLAIN (COSTS OFF) SELECT count(*) FROM ts a TABLESAMPLE system_time (20), ts b WHERE a.id = b.id;
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT * FROM ts TABLESAMPLE system_time (20) OFFSET 0) s;
EXPLAIN (COSTS OFF) SELECT (SELECT count(*) FROM ts TABLESAMPLE system_time (20));
DROP EXTENSION tsm_system_time;
DROP TABLE ts;
