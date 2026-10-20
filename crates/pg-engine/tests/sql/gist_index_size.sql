-- GiST index sizes: the sorted build point_ops and range_ops get (z-order or range order,
-- four pages a level split again by picksplit), inserts after the build (gistchoose,
-- page splits and their downlinks), exclusion constraints, TRUNCATE and empty indexes.
create table gst1 (id int, p point);
insert into gst1 select g, point(g % 100, g / 100) from generate_series(1, 10000) g;
create index gst1_p on gst1 using gist (p);
select pg_relation_size('gst1_p') / 8192 as pages;
create index gst1_part on gst1 using gist (p) where id % 2 = 0;
select pg_relation_size('gst1_part') / 8192 as pages;
create table gst2 (p point);
insert into gst2 select point((g * 7919) % 1000, (g::int8 * 104729) % 997) from generate_series(1, 30000) g;
create index gst2_p on gst2 using gist (p);
select pg_relation_size('gst2_p') / 8192 as pages;
create table gst3 (p point);
insert into gst3 select case when g % 5 = 0 then null else point(g * 0.37, -g * 1.5) end from generate_series(1, 20000) g;
create index gst3_p on gst3 using gist (p);
select pg_relation_size('gst3_p') / 8192 as pages;
create table gst4 (r int4range);
insert into gst4 select int4range(g, g + 10) from generate_series(1, 10000) g;
create index gst4_r on gst4 using gist (r);
select pg_relation_size('gst4_r') / 8192 as pages;
create table gst5 (r int8range);
insert into gst5 select case when g % 50 = 0 then 'empty'::int8range when g % 37 = 0 then int8range(g, null) when g % 41 = 0 then null else int8range((g * 7919) % 5000, (g * 7919) % 5000 + g % 300) end from generate_series(1, 40000) g;
create index gst5_r on gst5 using gist (r);
select pg_relation_size('gst5_r') / 8192 as pages;
create table gst6 (r daterange);
insert into gst6 select daterange('2020-01-01'::date + g % 400, '2020-01-01'::date + g % 400 + g % 7 + 1, '[]') from generate_series(1, 25000) g;
create index gst6_r on gst6 using gist (r);
select pg_relation_size('gst6_r') / 8192 as pages;
create table gst7 (r int4range);
insert into gst7 select int4range(null, g) from generate_series(1, 3000) g;
insert into gst7 select int4range(g, null) from generate_series(1, 3000) g;
create index gst7_r on gst7 using gist (r);
select pg_relation_size('gst7_r') / 8192 as pages;
-- Inserted after the index was made.
create table gst8 (r int4range);
create index gst8_r on gst8 using gist (r);
insert into gst8 select int4range(g, g + 10) from generate_series(1, 10000) g;
select pg_relation_size('gst8_r') / 8192 as pages;
create table gst9 (p point);
create index gst9_p on gst9 using gist (p);
insert into gst9 select point(g % 100, g / 100) from generate_series(1, 10000) g;
select pg_relation_size('gst9_p') / 8192 as pages;
create table gst10 (id int, during tsrange, exclude using gist (during with &&));
insert into gst10 select g, tsrange('2024-01-01'::timestamp + make_interval(hours => g), '2024-01-01'::timestamp + make_interval(hours => g + 1)) from generate_series(1, 2500) g;
select pg_relation_size('gst10_during_excl') / 8192 as pages;
create table gst11 (r daterange, exclude using gist (r with &&));
insert into gst11 select daterange('2000-01-01'::date + g * 3, '2000-01-01'::date + g * 3 + 2) from generate_series(1, 2000) g;
select pg_relation_size('gst11_r_excl') / 8192 as pages;
-- TRUNCATE leaves an empty index the rows go into one by one.
truncate gst1;
select pg_relation_size('gst1_p') / 8192 as pages;
insert into gst1 select g, point(g, -g) from generate_series(1, 3000) g;
select pg_relation_size('gst1_p') / 8192 as pages;
create table gst12 (p point);
create index gst12_p on gst12 using gist (p);
select pg_relation_size('gst12_p') as bytes;
drop table gst1, gst2, gst3, gst4, gst5, gst6, gst7, gst8, gst9, gst10, gst11, gst12;
