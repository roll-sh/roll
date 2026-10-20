-- Partitioned keys: a partition's own key attached to its parent's (conislocal), keys
-- made as copies of the parent's (connoinherit), invalid indexes on ATTACH and CREATE
-- INDEX, and the error of a second primary key on a partition.
CREATE TABLE grg_ip (i int) PARTITION BY HASH (i);
CREATE TABLE grg_ip0 PARTITION OF grg_ip (i) FOR VALUES WITH (MODULUS 2, REMAINDER 0);
CREATE TABLE grg_ip1 PARTITION OF grg_ip (i) FOR VALUES WITH (MODULUS 2, REMAINDER 1);
ALTER TABLE grg_ip0 ADD PRIMARY KEY (i);
ALTER TABLE grg_ip ADD PRIMARY KEY (i);
SELECT indrelid::regclass, indexrelid::regclass, inhparent::regclass, indisvalid, conname, conislocal, coninhcount, connoinherit, convalidated
  FROM pg_index idx LEFT JOIN pg_inherits inh ON (idx.indexrelid = inh.inhrelid)
  LEFT JOIN pg_constraint con ON (idx.indexrelid = con.conindid)
  WHERE indrelid::regclass::text LIKE 'grg_ip%'
  ORDER BY indexrelid::regclass::text COLLATE "C";
ALTER TABLE grg_ip0 DROP CONSTRAINT grg_ip0_pkey;
ALTER TABLE grg_ip DROP CONSTRAINT grg_ip_pkey;
DROP TABLE grg_ip;
CREATE TABLE grg_ip (a int) PARTITION BY RANGE (a);
CREATE TABLE grg_ip0 (LIKE grg_ip);
ALTER TABLE grg_ip0 ADD PRIMARY KEY (a);
ALTER TABLE grg_ip ATTACH PARTITION grg_ip0 FOR VALUES FROM (0) TO (1000);
ALTER TABLE ONLY grg_ip ADD PRIMARY KEY (a);
ALTER INDEX grg_ip_pkey ATTACH PARTITION grg_ip0_pkey;
SELECT indrelid::regclass, indexrelid::regclass, inhparent::regclass, indisvalid, conname, conislocal, coninhcount, connoinherit, convalidated
  FROM pg_index idx LEFT JOIN pg_inherits inh ON (idx.indexrelid = inh.inhrelid)
  LEFT JOIN pg_constraint con ON (idx.indexrelid = con.conindid)
  WHERE indrelid::regclass::text LIKE 'grg_ip%'
  ORDER BY indexrelid::regclass::text COLLATE "C";
DROP TABLE grg_ip;
CREATE TABLE grg_ref (a int PRIMARY KEY);
CREATE TABLE grg_pp (a int REFERENCES grg_ref, b int, c int, UNIQUE (a, b), EXCLUDE USING btree (a WITH =, c WITH =)) PARTITION BY RANGE (a);
CREATE TABLE grg_pp1 PARTITION OF grg_pp FOR VALUES FROM (0) TO (10);
CREATE TABLE grg_pp2 (a int REFERENCES grg_ref, b int, c int, UNIQUE (a, b), EXCLUDE USING btree (a WITH =, c WITH =));
ALTER TABLE grg_pp ATTACH PARTITION grg_pp2 FOR VALUES FROM (10) TO (20);
SELECT conrelid::regclass, conname, contype, conislocal, coninhcount, connoinherit FROM pg_constraint
  WHERE conrelid::regclass::text LIKE 'grg_%' AND contype <> 'n' ORDER BY conrelid::regclass::text, conname;
ALTER TABLE grg_pp DETACH PARTITION grg_pp2;
SELECT conrelid::regclass, conname, contype, conislocal, coninhcount, connoinherit FROM pg_constraint
  WHERE conrelid::regclass::text LIKE 'grg_pp2%' AND contype <> 'n' ORDER BY conname;
DROP TABLE grg_pp, grg_pp2, grg_ref;
-- an invalid index isn't taken when attaching a partition
CREATE TABLE grg_inv (a int) PARTITION BY RANGE (a);
CREATE INDEX grg_inv_idx ON grg_inv (a);
CREATE TABLE grg_inv_1 (a int) PARTITION BY RANGE (a);
CREATE TABLE grg_inv_1_1 PARTITION OF grg_inv_1 FOR VALUES FROM (0) TO (10);
CREATE TABLE grg_inv_1_2 PARTITION OF grg_inv_1 FOR VALUES FROM (10) TO (20);
CREATE INDEX grg_inv_ixd_1 ON ONLY grg_inv_1 (a);
ALTER TABLE grg_inv ATTACH PARTITION grg_inv_1 FOR VALUES FROM (1) TO (100);
SELECT indexrelid::regclass, indisvalid, indrelid::regclass, inhparent::regclass
  FROM pg_index idx LEFT JOIN pg_inherits inh ON (idx.indexrelid = inh.inhrelid)
  WHERE indexrelid::regclass::text LIKE 'grg_inv%'
  ORDER BY indexrelid::regclass::text COLLATE "C";
DROP TABLE grg_inv;
-- CREATE INDEX on the parent takes an invalid one, invalid up the tree then
CREATE TABLE grg_isv (a int, b int) PARTITION BY RANGE (a);
CREATE TABLE grg_isv_1 PARTITION OF grg_isv FOR VALUES FROM (1) TO (10) PARTITION BY RANGE (a);
CREATE TABLE grg_isv_2 PARTITION OF grg_isv FOR VALUES FROM (10) TO (20) PARTITION BY RANGE (a);
CREATE TABLE grg_isv_11 PARTITION OF grg_isv_1 FOR VALUES FROM (1) TO (5);
CREATE TABLE grg_isv_12 PARTITION OF grg_isv_1 FOR VALUES FROM (5) TO (10);
INSERT INTO grg_isv_11 VALUES (1, 0);
CREATE INDEX CONCURRENTLY grg_isv_idx_11 ON grg_isv_11 ((a/b));
CREATE INDEX grg_isv_idx ON grg_isv ((a/b));
SELECT indexrelid::regclass, indisvalid, indrelid::regclass, inhparent::regclass
  FROM pg_index idx LEFT JOIN pg_inherits inh ON (idx.indexrelid = inh.inhrelid)
  WHERE indexrelid::regclass::text LIKE 'grg_isv%'
  ORDER BY indexrelid::regclass::text COLLATE "C";
DROP TABLE grg_isv;
-- a second primary key on a partition: no position (index_check_primary_key)
CREATE TABLE grg_mp (a int PRIMARY KEY, b int) PARTITION BY RANGE (a);
CREATE TABLE grg_mp1 PARTITION OF grg_mp (b PRIMARY KEY) FOR VALUES FROM (0) TO (100);
CREATE TABLE grg_mp2 (a int PRIMARY KEY, b int PRIMARY KEY);
DROP TABLE grg_mp;
