-- agent 279: a catalog's size as estimate_rel_size sees it: reltuples / relpages times its
-- pages (rows added on initdb's last pages leave initdb's reltuples), a catalog initdb left
-- empty at its width's density; after VACUUM and ANALYZE their counts; an index-only scan
-- reads the heap for the pages not all-visible.
create role a279r1;
create role a279r2;
explain select rolname from pg_authid where rolname like 'a279%';
create table a279s (a int);
create function a279tf() returns trigger language plpgsql as $$ begin return new; end $$;
create trigger a279s1 before insert on a279s for each row execute function a279tf();
create trigger a279s2 before insert on a279s for each row execute function a279tf();
explain select tgname from pg_trigger where tgname like 'a279s%';
create type a279e as enum ('a', 'b', 'c');
explain select count(*) from pg_enum where enumlabel = 'b';
explain select oid from pg_class where oid = 1259;
analyze pg_trigger;
explain select tgname from pg_trigger where tgname like 'a279s%';
drop role a279r1;
drop role a279r2;
