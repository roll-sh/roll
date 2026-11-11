-- "regular expression is too complex" from the regex code's stack depth checks: Postgres
-- 18.6 fails when its C stack passes max_stack_depth (2MB); the engine emulates that stack
-- (each recursive function's frame as Postgres's build lays it out), so the same patterns
-- fail. Nesting (parse), long chains (NFA traversals), captures (branch recursion), and
-- nested lookaround (the executor), each just below and at the limit. The sizes are the
-- x86-64 reference's boundaries (the engine emulates the build of the host's architecture: on
-- aarch64, re-record this file from an aarch64 reference with sizes at its boundaries).
select 'a' ~ (repeat('(', 8710) || 'a' || repeat(')', 8710));
select 'a' ~ (repeat('(', 8711) || 'a' || repeat(')', 8711));
select 'a' ~ repeat('a', 43549);
select 'a' ~ repeat('a', 43550);
select 'a' ~ repeat('(a)', 13065);
select 'a' ~ repeat('(a)', 13066);
select 'a' ~ (repeat('(', 1308) || 'a' || repeat(')*', 1308));
select 'a' ~ (repeat('(', 1309) || 'a' || repeat(')*', 1309));
select 'a' ~ (repeat('(?=', 6121) || 'a' || repeat(')', 6121));
select 'a' ~ (repeat('(?=', 6122) || 'a' || repeat(')', 6122));
select regexp_match('a', repeat('(', 8711) || 'a' || repeat(')', 8711));
select regexp_replace('a', repeat('a', 43550), 'x');
-- statement_timeout reaches the regex functions (CHECK_FOR_INTERRUPTS between matches,
-- INTERRUPT in the compiler and executor).
set statement_timeout = '50ms';
select length(regexp_replace(repeat('a', 5000000), 'a', 'b', 'g'));
select count(*) from regexp_matches(repeat('a', 2000000), 'a', 'g');
reset statement_timeout;
select length(regexp_replace(repeat('a', 50000), 'a', 'b', 'g'));
