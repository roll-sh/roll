-- ORM-style queries over the TPC-H schema (tools/bench/tpch.rs), as each ORM writes them
-- (literals for bind parameters). tools/bench/plancmp.py --sets orm.

-- name: django_get
SELECT "orders"."o_orderkey", "orders"."o_custkey", "orders"."o_orderstatus", "orders"."o_totalprice", "orders"."o_orderdate" FROM "orders" WHERE "orders"."o_orderkey" = 4242 LIMIT 21;
-- name: django_count_filter
SELECT COUNT(*) AS "__count" FROM "orders" WHERE "orders"."o_custkey" = 1234;
-- name: django_exists
SELECT 1 AS "a" FROM "orders" WHERE "orders"."o_custkey" = 777 LIMIT 1;
-- name: django_page
SELECT "orders"."o_orderkey", "orders"."o_orderdate", "orders"."o_totalprice" FROM "orders" WHERE "orders"."o_orderstatus" = 'F' ORDER BY "orders"."o_orderdate" DESC LIMIT 25 OFFSET 50;
-- name: django_prefetch
SELECT "orders"."o_orderkey", "orders"."o_custkey", "orders"."o_totalprice" FROM "orders" WHERE "orders"."o_custkey" IN (1, 2, 4, 5, 7, 8, 10, 11, 13, 14, 16, 17, 19, 20, 22);
-- name: django_annotate_count
SELECT "customer"."c_custkey", "customer"."c_name", COUNT("orders"."o_orderkey") AS "n" FROM "customer" LEFT OUTER JOIN "orders" ON ("customer"."c_custkey" = "orders"."o_custkey") WHERE "customer"."c_nationkey" = 3 GROUP BY "customer"."c_custkey" ORDER BY "n" DESC LIMIT 21;
-- name: django_exists_subquery
SELECT "orders"."o_orderkey", "orders"."o_totalprice" FROM "orders" WHERE EXISTS(SELECT 1 AS "a" FROM "lineitem" U0 WHERE (U0."l_orderkey" = ("orders"."o_orderkey") AND U0."l_quantity" > 49) LIMIT 1) ORDER BY "orders"."o_orderkey" ASC LIMIT 21;
-- name: django_distinct_count
SELECT COUNT(*) FROM (SELECT DISTINCT "orders"."o_custkey" AS "col1" FROM "orders" WHERE "orders"."o_orderdate" >= '1998-01-01'::date) subquery;
-- name: django_values_aggregate
SELECT "orders"."o_orderpriority", SUM("orders"."o_totalprice") AS "total", COUNT("orders"."o_orderkey") AS "n" FROM "orders" WHERE "orders"."o_orderdate" BETWEEN '1995-01-01'::date AND '1995-03-31'::date GROUP BY "orders"."o_orderpriority" ORDER BY "orders"."o_orderpriority" ASC;
-- name: django_select_related
SELECT "orders"."o_orderkey", "orders"."o_totalprice", "customer"."c_custkey", "customer"."c_name", "nation"."n_name" FROM "orders" INNER JOIN "customer" ON ("orders"."o_custkey" = "customer"."c_custkey") INNER JOIN "nation" ON ("customer"."c_nationkey" = "nation"."n_nationkey") WHERE "orders"."o_orderkey" = 98765 LIMIT 21;
-- name: django_subquery_annotation
SELECT "customer"."c_custkey", (SELECT U0."o_orderdate" FROM "orders" U0 WHERE U0."o_custkey" = ("customer"."c_custkey") ORDER BY U0."o_orderdate" DESC LIMIT 1) AS "last_order" FROM "customer" WHERE "customer"."c_nationkey" = 7 ORDER BY "customer"."c_custkey" ASC LIMIT 21;
-- name: rails_find
SELECT "orders".* FROM "orders" WHERE "orders"."o_orderkey" = 31337 LIMIT 1;
-- name: rails_where_order
SELECT "orders".* FROM "orders" WHERE "orders"."o_custkey" = 4321 ORDER BY "orders"."o_orderkey" ASC LIMIT 20;
-- name: rails_exists
SELECT 1 AS one FROM "lineitem" WHERE "lineitem"."l_orderkey" = 7 LIMIT 1;
-- name: rails_includes
SELECT "customer".* FROM "customer" WHERE "customer"."c_custkey" IN (37, 46, 49, 52, 56, 61, 62, 64, 67, 70);
-- name: rails_joins_count
SELECT COUNT(*) FROM "orders" INNER JOIN "customer" ON "customer"."c_custkey" = "orders"."o_custkey" WHERE "customer"."c_mktsegment" = 'BUILDING' AND "orders"."o_orderdate" >= '1998-06-01';
-- name: rails_find_each
SELECT "orders".* FROM "orders" WHERE "orders"."o_orderkey" > 600000 ORDER BY "orders"."o_orderkey" ASC LIMIT 1000;
-- name: rails_group_count
SELECT COUNT(*) AS "count_all", "lineitem"."l_returnflag" AS "lineitem_l_returnflag" FROM "lineitem" WHERE "lineitem"."l_shipdate" > '1998-11-01' GROUP BY "lineitem"."l_returnflag";
-- name: rails_pluck_distinct
SELECT DISTINCT "orders"."o_orderpriority" FROM "orders";
-- name: rails_left_outer_joins_missing
SELECT "customer".* FROM "customer" LEFT OUTER JOIN "orders" ON "orders"."o_custkey" = "customer"."c_custkey" WHERE "orders"."o_orderkey" IS NULL AND "customer"."c_nationkey" = 12 LIMIT 50;
-- name: prisma_find_many
SELECT "public"."orders"."o_orderkey", "public"."orders"."o_custkey", "public"."orders"."o_totalprice" FROM "public"."orders" WHERE ("public"."orders"."o_custkey" = 2222 AND 1=1) ORDER BY "public"."orders"."o_orderkey" ASC LIMIT 10 OFFSET 0;
-- name: prisma_count
SELECT COUNT(*) FROM (SELECT "public"."orders"."o_orderkey" FROM "public"."orders" WHERE "public"."orders"."o_orderstatus" = 'F' OFFSET 0) AS "sub";
-- name: prisma_count_all
SELECT COUNT(*) AS "_count._all" FROM (SELECT "public"."lineitem"."l_orderkey" FROM "public"."lineitem" WHERE 1=1 OFFSET 0) AS "sub";
-- name: prisma_relation_in
SELECT "public"."lineitem"."l_orderkey", "public"."lineitem"."l_linenumber", "public"."lineitem"."l_quantity" FROM "public"."lineitem" WHERE "public"."lineitem"."l_orderkey" IN (1, 2, 3, 4, 5, 6, 7, 32, 33, 34, 35, 36, 37, 38, 39, 64) OFFSET 0;
-- name: prisma_lateral_join
SELECT "t1"."c_custkey", "t1"."c_name", "customer_orders"."__prisma_data__" AS "orders" FROM "public"."customer" AS "t1" LEFT JOIN LATERAL (SELECT COALESCE(JSONB_AGG("__prisma_data__"), '[]') AS "__prisma_data__" FROM (SELECT "t4"."__prisma_data__" FROM (SELECT JSONB_BUILD_OBJECT('o_orderkey', "t3"."o_orderkey", 'o_totalprice', "t3"."o_totalprice") AS "__prisma_data__", "t3"."o_orderdate" FROM (SELECT "t2".* FROM "public"."orders" AS "t2" WHERE "t1"."c_custkey" = "t2"."o_custkey") AS "t3" ORDER BY "t3"."o_orderdate" DESC LIMIT 5) AS "t4") AS "t5") AS "customer_orders" ON true WHERE "t1"."c_nationkey" = 9 ORDER BY "t1"."c_custkey" ASC LIMIT 10 OFFSET 0;
-- name: prisma_group_by
SELECT "public"."orders"."o_orderstatus", COUNT("public"."orders"."o_orderkey") AS "_count$o_orderkey", SUM("public"."orders"."o_totalprice") AS "_sum$o_totalprice" FROM "public"."orders" WHERE 1=1 GROUP BY "public"."orders"."o_orderstatus" OFFSET 0;
-- name: efcore_page
SELECT o.o_orderkey, o.o_custkey, o.o_totalprice FROM orders AS o WHERE o.o_totalprice > 450000.0 ORDER BY o.o_orderkey LIMIT 20 OFFSET 40;
-- name: efcore_any
SELECT EXISTS (SELECT 1 FROM orders AS o WHERE o.o_custkey = 5);
-- name: efcore_where_any
SELECT o.o_orderkey, o.o_orderdate FROM orders AS o WHERE o.o_orderdate >= DATE '1998-07-01' AND EXISTS (SELECT 1 FROM lineitem AS l WHERE o.o_orderkey = l.l_orderkey AND l.l_discount > 0.09);
-- name: efcore_include_collection
SELECT c0.c_custkey, c0.c_name, o.o_orderkey, o.o_totalprice FROM (SELECT c.c_custkey, c.c_name FROM customer AS c WHERE c.c_nationkey = 1 ORDER BY c.c_custkey LIMIT 10) AS c0 LEFT JOIN orders AS o ON c0.c_custkey = o.o_custkey ORDER BY c0.c_custkey, o.o_orderkey;
-- name: efcore_count_distinct
SELECT COUNT(*)::int FROM (SELECT DISTINCT o.o_custkey FROM orders AS o WHERE o.o_orderpriority = '1-URGENT') AS o0;
-- name: efcore_first_or_default
SELECT c.c_custkey, c.c_name, c.c_acctbal FROM customer AS c WHERE c.c_name = 'Customer#000012345' LIMIT 1;
-- name: efcore_select_many
SELECT c.c_custkey, o0.o_orderkey FROM customer AS c LEFT JOIN LATERAL (SELECT o.o_orderkey FROM orders AS o WHERE c.c_custkey = o.o_custkey ORDER BY o.o_totalprice DESC LIMIT 2) AS o0 ON TRUE WHERE c.c_acctbal > 9990.0;
-- name: hibernate_find
select o1_0.o_orderkey,o1_0.o_clerk,o1_0.o_comment,o1_0.o_custkey,o1_0.o_orderdate,o1_0.o_orderpriority,o1_0.o_orderstatus,o1_0.o_shippriority,o1_0.o_totalprice from orders o1_0 where o1_0.o_orderkey=1000000;
-- name: hibernate_fetch_first
select o1_0.o_orderkey,o1_0.o_orderdate from orders o1_0 where o1_0.o_custkey=3333 order by o1_0.o_orderdate desc fetch first 5 rows only;
-- name: hibernate_join_fetch
select o1_0.o_orderkey,c1_0.c_custkey,c1_0.c_name from orders o1_0 join customer c1_0 on c1_0.c_custkey=o1_0.o_custkey where c1_0.c_name='Customer#000000100';
-- name: hibernate_count
select count(o1_0.o_orderkey) from orders o1_0 where o1_0.o_orderstatus='P';
-- name: hibernate_offset_fetch
select l1_0.l_orderkey,l1_0.l_linenumber,l1_0.l_shipdate from lineitem l1_0 where l1_0.l_shipdate between '1996-01-01' and '1996-01-31' order by l1_0.l_shipdate offset 100 rows fetch first 50 rows only;
-- name: hibernate_in_subquery
select c1_0.c_custkey,c1_0.c_name from customer c1_0 where c1_0.c_custkey in (select o1_0.o_custkey from orders o1_0 where o1_0.o_totalprice>500000);
-- name: sqlalchemy_count
SELECT count(*) AS count_1 FROM (SELECT orders.o_orderkey AS orders_o_orderkey, orders.o_custkey AS orders_o_custkey FROM orders WHERE orders.o_orderstatus = 'O') AS anon_1;
-- name: sqlalchemy_selectinload
SELECT orders.o_custkey AS orders_o_custkey, orders.o_orderkey AS orders_o_orderkey, orders.o_totalprice AS orders_o_totalprice FROM orders WHERE orders.o_custkey IN (100, 200, 300, 400, 500, 600, 700, 800, 900, 1000) ORDER BY orders.o_custkey;
-- name: sqlalchemy_subqueryload
SELECT orders.o_orderkey AS orders_o_orderkey, anon_1.customer_c_custkey AS anon_1_customer_c_custkey FROM (SELECT customer.c_custkey AS customer_c_custkey FROM customer WHERE customer.c_nationkey = 3 AND customer.c_acctbal > 9900) AS anon_1 JOIN orders ON anon_1.customer_c_custkey = orders.o_custkey ORDER BY anon_1.customer_c_custkey;
-- name: sqlalchemy_limit_offset_wrap
SELECT anon_1.o_orderkey, anon_1.o_totalprice FROM (SELECT orders.o_orderkey AS o_orderkey, orders.o_totalprice AS o_totalprice FROM orders WHERE orders.o_orderpriority = '2-HIGH' ORDER BY orders.o_totalprice DESC LIMIT 10 OFFSET 0) AS anon_1 ORDER BY anon_1.o_totalprice DESC;
-- name: keyset_page
SELECT o_orderkey, o_orderdate, o_totalprice FROM orders WHERE (o_orderdate, o_orderkey) > ('1996-06-01', 123456) ORDER BY o_orderdate, o_orderkey LIMIT 50;
-- name: exists_limit_order_limit
SELECT c.c_custkey, c.c_name FROM customer c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.o_custkey = c.c_custkey AND o.o_orderstatus = 'P' LIMIT 1) ORDER BY c.c_acctbal DESC LIMIT 21;
-- name: join_order_date_offset
SELECT o.o_orderkey, o.o_orderdate, c.c_name FROM orders o JOIN customer c ON c.c_custkey = o.o_custkey WHERE c.c_mktsegment = 'MACHINERY' ORDER BY o.o_orderdate DESC OFFSET 100 LIMIT 20;
-- name: cte_window
WITH t AS (SELECT o_custkey, sum(o_totalprice) AS v FROM orders WHERE o_orderdate >= '1998-01-01' GROUP BY o_custkey) SELECT o_custkey, v, rank() OVER (ORDER BY v DESC) FROM t LIMIT 10;
-- name: union_all_sorted
SELECT k FROM (SELECT o_custkey AS k FROM orders WHERE o_orderkey < 1000 UNION ALL SELECT c_custkey FROM customer WHERE c_custkey < 500) u ORDER BY k LIMIT 30;
-- name: group_three_way
SELECT n.n_name, count(*), sum(o.o_totalprice) FROM orders o JOIN customer c ON c.c_custkey = o.o_custkey JOIN nation n ON n.n_nationkey = c.c_nationkey WHERE o.o_orderdate >= '1997-01-01' GROUP BY n.n_name ORDER BY n.n_name;
