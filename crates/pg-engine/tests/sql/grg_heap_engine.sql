-- Heap layout modelled after the regression sweep: an AFTER ROW trigger's tuple slot
-- keeps its page from being pruned, index scans mark dead entries (kill_prior_tuple),
-- a rolled back TRUNCATE ... RESTART IDENTITY, CLUSTER through a sort.
CREATE TABLE grg_x (a serial, b int, c text NOT NULL DEFAULT 'stuff', d text, e text);
CREATE FUNCTION grg_x_before() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN NEW.e := 'before trigger fired'; RETURN NEW; END $$;
CREATE FUNCTION grg_x_after() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN UPDATE grg_x SET e = 'after trigger fired' WHERE c = 'stuff'; RETURN NULL; END $$;
CREATE TRIGGER grg_x_after AFTER INSERT ON grg_x FOR EACH ROW EXECUTE PROCEDURE grg_x_after();
CREATE TRIGGER grg_x_before BEFORE INSERT ON grg_x FOR EACH ROW EXECUTE PROCEDURE grg_x_before();
COPY grg_x (a, b, c, d, e) FROM STDIN;
9999	\N	\\N	\NN	\N
10000	21	31	41	51
\.
COPY grg_x (b, d) FROM STDIN;
1	test_1
\.
COPY grg_x (b, d) FROM STDIN;
2	test_2
3	test_3
4	test_4
5	test_5
\.
COPY grg_x (a, b, c, d, e) FROM STDIN;
10001	22	32	42	52
10002	23	33	43	53
10003	24	34	44	54
10004	25	35	45	55
10005	26	36	46	56
\.
COPY grg_x (b, c, d, e) FROM STDIN DELIMITER ',' NULL 'x';
x,45,80,90
x,\x,\\x,\\\x
x,\,,\\\,,\\
\.
COPY grg_x FROM STDIN WITH DELIMITER AS ';' NULL AS '';
3000;;c;;
\.
COPY grg_x FROM STDIN WITH DELIMITER AS ':' NULL AS E'\\X' ENCODING 'sql_ascii';
4000:\X:C:\X:\X
4001:1:empty::
4002:2:null:\X:\X
4003:3:Backslash:\\:\\
4004:4:BackslashX:\\X:\\X
4005:5:N:\N:\N
4006:6:BackslashN:\\N:\\N
4007:7:XX:\XX:\XX
4008:8:Delimiter:\::\:
\.
COPY grg_x FROM STDIN WHERE a = 50004;
50003	24	34	44	54
50004	25	35	45	55
50005	26	36	46	56
\.
COPY grg_x FROM STDIN WHERE a > 60003;
60001	22	32	42	52
60002	23	33	43	53
60003	24	34	44	54
60004	25	35	45	55
60005	26	36	46	56
\.
SELECT ctid, * FROM grg_x;
-- index scans mark the entries of an aborted subtransaction's rows dead
BEGIN;
SET LOCAL enable_seqscan = false;
SET LOCAL enable_indexonlyscan = false;
SET LOCAL enable_bitmapscan = false;
CREATE TABLE grg_clean (key int, data text);
CREATE INDEX grg_clean_key ON grg_clean (key);
INSERT INTO grg_clean (key, data) VALUES (-1, 'just to allocate metapage');
SELECT pg_relation_size('grg_clean_key');
DO $$
BEGIN
    FOR i IN 1..100 LOOP
        BEGIN
            IF EXISTS (SELECT * FROM grg_clean WHERE key > 0 AND key < 100) THEN
                RAISE data_corrupted USING MESSAGE = 'these rows should not exist';
            END IF;
            INSERT INTO grg_clean SELECT g.i, 'rolling back in a sec' FROM generate_series(1, 100) g(i);
            RAISE reading_sql_data_not_permitted USING MESSAGE = 'round and round again';
        EXCEPTION WHEN reading_sql_data_not_permitted THEN END;
    END LOOP;
END $$;
SELECT pg_relation_size('grg_clean_key');
ROLLBACK;
-- a rolled back RESTART IDENTITY gives the sequences back their state
CREATE SEQUENCE grg_ta_id1 START WITH 33;
CREATE TABLE grg_ta (id serial, id1 integer DEFAULT nextval('grg_ta_id1'));
ALTER SEQUENCE grg_ta_id1 OWNED BY grg_ta.id1;
INSERT INTO grg_ta DEFAULT VALUES;
INSERT INTO grg_ta DEFAULT VALUES;
BEGIN;
TRUNCATE grg_ta RESTART IDENTITY;
INSERT INTO grg_ta DEFAULT VALUES;
SELECT ctid, * FROM grg_ta;
ROLLBACK;
INSERT INTO grg_ta DEFAULT VALUES;
INSERT INTO grg_ta DEFAULT VALUES;
SELECT ctid, * FROM grg_ta;
BEGIN;
SAVEPOINT s1;
TRUNCATE grg_ta RESTART IDENTITY;
INSERT INTO grg_ta DEFAULT VALUES;
ROLLBACK TO s1;
INSERT INTO grg_ta DEFAULT VALUES;
COMMIT;
SELECT ctid, * FROM grg_ta;
-- CLUSTER through a sequential scan and a sort: equal keys in quicksort's order
CREATE TABLE grg_cl (id int, k int);
INSERT INTO grg_cl SELECT g, g % 7 FROM generate_series(1, 300) g;
INSERT INTO grg_cl SELECT g, NULL FROM generate_series(301, 305) g;
CREATE INDEX grg_cl_k ON grg_cl (k);
BEGIN;
SET LOCAL enable_indexscan = false;
CLUSTER grg_cl USING grg_cl_k;
SELECT ctid, * FROM grg_cl ORDER BY ctid LIMIT 12;
SELECT ctid, * FROM grg_cl ORDER BY ctid DESC LIMIT 8;
ROLLBACK;
SELECT ctid, * FROM grg_cl ORDER BY ctid LIMIT 3;
DROP TABLE grg_x, grg_ta, grg_cl;
DROP FUNCTION grg_x_before();
DROP FUNCTION grg_x_after();
