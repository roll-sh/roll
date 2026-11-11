create table gx_rs_upsert (key int4 primary key, color text);

create function gx_rs_upsert_before_func()
  returns trigger language plpgsql as
$$
begin
  if (TG_OP = 'UPDATE') then
    raise warning 'before update (old): %', old.*::text;
    raise warning 'before update (new): %', new.*::text;
  elsif (TG_OP = 'INSERT') then
    raise warning 'before insert (new): %', new.*::text;
    if new.key % 2 = 0 then
      new.key := new.key + 1;
      new.color := new.color || ' trig modified';
      raise warning 'before insert (new, modified): %', new.*::text;
    end if;
  end if;
  return new;
end;
$$;
create trigger gx_rs_upsert_before_trig before insert or update on gx_rs_upsert
  for each row execute procedure gx_rs_upsert_before_func();

create function gx_rs_upsert_after_func()
  returns trigger language plpgsql as
$$
begin
  if (TG_OP = 'UPDATE') then
    raise warning 'after update (old): %', old.*::text;
    raise warning 'after update (new): %', new.*::text;
  elsif (TG_OP = 'INSERT') then
    raise warning 'after insert (new): %', new.*::text;
  end if;
  return null;
end;
$$;
create trigger gx_rs_upsert_after_trig after insert or update on gx_rs_upsert
  for each row execute procedure gx_rs_upsert_after_func();

insert into gx_rs_upsert values(1, 'black') on conflict (key) do update set color = 'updated ' || gx_rs_upsert.color;
insert into gx_rs_upsert values(2, 'red') on conflict (key) do update set color = 'updated ' || gx_rs_upsert.color;
insert into gx_rs_upsert values(3, 'orange') on conflict (key) do update set color = 'updated ' || gx_rs_upsert.color;
insert into gx_rs_upsert values(4, 'green') on conflict (key) do update set color = 'updated ' || gx_rs_upsert.color;
insert into gx_rs_upsert values(5, 'purple') on conflict (key) do update set color = 'updated ' || gx_rs_upsert.color;
insert into gx_rs_upsert values(6, 'white') on conflict (key) do update set color = 'updated ' || gx_rs_upsert.color;
insert into gx_rs_upsert values(7, 'pink') on conflict (key) do update set color = 'updated ' || gx_rs_upsert.color;
insert into gx_rs_upsert values(8, 'yellow') on conflict (key) do update set color = 'updated ' || gx_rs_upsert.color;

drop table gx_rs_upsert; drop function gx_rs_upsert_before_func(); drop function gx_rs_upsert_after_func();
