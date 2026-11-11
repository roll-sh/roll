-- ALTER COLUMN TYPE without a rewrite re-creates the column's indexes, each keeping its file
-- when CheckIndexCompatible passes: no expressions or predicate, the same operator classes
-- and collations, no polymorphic operator class over a changed type, and the same exclusion
-- operators.
create table cix_t (a varchar(10), b varchar(10), c varchar(10), d varchar(10), e varchar(10), g text, h varchar(10), k varchar(10), m varchar(10), n text, p text collate "C", q text collate "C", r varchar(10));
create index cix_a on cix_t (a);
create index cix_b_part on cix_t (b) where b > 'm';
create index cix_c_expr on cix_t (lower(c));
create index cix_d_coll on cix_t (d collate "C");
create index cix_e_incl on cix_t (g) include (e);
create index cix_h_hash on cix_t using hash (h);
alter table cix_t add constraint cix_ex exclude using btree (k with =);
create index cix_m_multi on cix_t (m, a);
create index cix_n on cix_t (n);
create index cix_g_desc on cix_t (g desc nulls last);
create index cix_p on cix_t (p);
create index cix_q_coll on cix_t (q collate "POSIX");
create unique index cix_r_uniq on cix_t (r) with (fillfactor = 50);
create temp table cix_files as select relname, pg_relation_filenode(oid) as fn from pg_class where relname like 'cix\_%' and relkind = 'i';
alter table cix_t alter a type varchar(20);
alter table cix_t alter b type varchar(20);
alter table cix_t alter c type varchar(20);
alter table cix_t alter d type text;
alter table cix_t alter e type varchar(20);
alter table cix_t alter h type text;
alter table cix_t alter k type text;
alter table cix_t alter m type text;
alter table cix_t alter n type text collate "C";
alter table cix_t alter p type varchar;
alter table cix_t alter q type varchar;
alter table cix_t alter r type varchar(30);
select attname, attcollation::regcollation from pg_attribute where attrelid = 'cix_t'::regclass and attname in ('n', 'p', 'q', 'r') order by attnum;
select f.relname, pg_relation_filenode(c.oid) = f.fn as kept from cix_files f join pg_class c on c.relname = f.relname order by 1;
drop table cix_t;
drop table cix_files;
