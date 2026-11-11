-- psql's backslash commands over the regression database (tests/pg_dump.rs, pg_psql_meta):
-- every command that queries the server, with the + and S variants, run on the engine and
-- on the reference and compared unit by unit.
\pset pager off
\l
\l+ regression*
\dn
\dn+
\dnS pg_*
\dt
\dt+ public.*
\dtS pg_catalog.pg_c*
\di
\di+ public.onek*
\diS pg_catalog.pg_class*
\ds
\ds+
\dv
\dv+ public.*
\dvS pg_catalog.pg_stat_a*
\dm
\dm+
\dE
\dE+
\d onek
\d+ onek
\d tenk1
\d+ tenk1
\d pg_class
\dS+ pg_class
\d+ pg_catalog.pg_stat_activity
\d onek_unique1
\d+ onek_unique1
\d+ mvtest_tm
\d road
\d+ ihighway
\d+ street
\d+ shoe_ready
\d+ rtest_vview3
\df
\df+ public.*
\dfa
\dfn public.*
\dfp
\dft
\dfw
\dfS+ pg_catalog.int4*
\df *.nth_value*
\da
\da+
\daS pg_catalog.sum
\dA
\dA+
\dAc
\dAc+ btree
\dAf
\dAf+ hash
\dAo btree integer_ops
\dAo+ gist
\dAp btree float_ops
\dAp+ hash
\db
\db+
\dc
\dc+
\dcS
\dC
\dC+ integer
\dd
\ddS
\dD
\dD+
\dDS
\dT
\dT+ public.*
\dTS+ pg_catalog.int4
\dT+ *range*
\dx
\dx+
\dX
\dX+
\dy
\dy+
\des
\des+
\deu
\deu+
\dew
\dew+
\det
\det+
\dF
\dF+
\dFd
\dFd+
\dFp
\dFp+
\dFt
\dFt+
\dg
\dg+
\du
\du+
\drg
\drgS
\drds
\dRp
\dRp+
\dRs
\dRs+
\dL
\dL+
\dLS
\dp
\dp public.*
\dpS pg_catalog.pg_class
\z public.onek*
\ddp
\dconfig
\dconfig work_mem
\dconfig+ enable*
\do
\do+ public.*
\doS+ pg_catalog.+
\do - integer integer
\sf fipshash(text)
\sf+ public.fipshash(bytea)
\sv shoe_ready
\sv+ rtest_vview3
\ef fipshash(text)
\p
\r
\ev shoe_ready
\p
\r
\dl
\dl+
\lo_list
\lo_list+
SELECT 1 AS one, 'x'::text AS two \gdesc
SELECT unique1, ten FROM onek WHERE unique1 < 5 \gdesc
SELECT ten, four, count(*) FROM tenk1 GROUP BY ten, four ORDER BY 1, 2 \crosstabview ten four
SELECT count(*) FROM onek \watch count=1
\copy (SELECT unique1, stringu1 FROM onek ORDER BY unique1 LIMIT 3) TO STDOUT
\copy (SELECT unique1, stringu1 FROM onek ORDER BY unique1 LIMIT 3) TO STDOUT WITH (FORMAT csv, HEADER)
\encoding
\set ECHO_HIDDEN on
\d+ int4_tbl
\set ECHO_HIDDEN off
