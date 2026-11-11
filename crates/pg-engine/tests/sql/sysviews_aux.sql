-- pg_stat_activity lists the server's auxiliary processes after the client backends; a role
-- without pg_read_all_stats sees no details of them. Also: an empty oidvector's dimensions.
select backend_type, datid is null as no_db, usesysid is null as no_role, application_name,
       state, query, wait_event_type, wait_event, client_port, backend_start is not null as started
  from pg_stat_activity where backend_type <> 'client backend' order by backend_type, pid;
select count(*) from pg_stat_activity where backend_type = 'checkpointer';
select pg_log_backend_memory_contexts(pid) from pg_stat_activity where backend_type = 'checkpointer';
select pg_stat_get_backend_activity(b) from pg_stat_get_backend_idset() b
  where pg_stat_get_backend_pid(b) in (select pid from pg_stat_activity where backend_type = 'walwriter');
create role regress_aux_viewer;
grant pg_signal_backend to regress_aux_viewer;
create function aux_terminate(int) returns bool language plpgsql set client_min_messages = error as $$
begin
  return pg_terminate_backend($1);
exception when others then
  return false;
end $$;
begin;
set local role regress_aux_viewer;
select count(*), count(backend_type) from pg_stat_activity where usesysid is null;
select backend_type from pg_stat_activity where coalesce(usesysid, 10) = 10 and aux_terminate(pid);
rollback;
drop function aux_terminate(int);
drop role regress_aux_viewer;
select array_dims(proargtypes) from pg_proc where oid = 'pg_backend_pid'::regproc;
select array_dims(proargtypes) from pg_proc where oid = 'pg_sleep'::regproc;
select array_dims(indkey) from pg_index where indexrelid = 'pg_class_oid_index'::regclass;
