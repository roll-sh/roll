-- pg_terminate_backend / pg_cancel_backend: no such backend is a WARNING; signalling a
-- superuser's backend takes a superuser.
select pg_terminate_backend(999999);
select pg_cancel_backend(999999);
select pg_terminate_backend(999999, 10);
select pg_terminate_backend(pg_backend_pid(), -1);
create role sbp_r1;
set role sbp_r1;
select pg_terminate_backend(pg_backend_pid());
select pg_cancel_backend(pg_backend_pid());
reset role;
create function sbp_terminate_nothrow(pid int) returns bool
  language plpgsql security definer set client_min_messages = error as $$
begin
  return pg_terminate_backend($1);
exception when others then
  return false;
end$$;
alter function sbp_terminate_nothrow owner to pg_signal_backend;
select sbp_terminate_nothrow(pg_backend_pid());
drop function sbp_terminate_nothrow(int);
drop role sbp_r1;
