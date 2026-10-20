-- "regular expression is too complex" from the regex code's stack depth checks: Postgres
-- 18.6 fails when its C stack passes max_stack_depth (2MB); the engine emulates that stack
-- (each recursive function's frame as Postgres's build lays it out), so the same patterns
-- fail. Nesting (parse), long chains (NFA traversals), captures (branch recursion), and
-- nested lookaround (the executor), each just below and at the limit.
select 'a' ~ (repeat('(', 7683) || 'a' || repeat(')', 7683));
select 'a' ~ (repeat('(', 7684) || 'a' || repeat(')', 7684));
select 'a' ~ repeat('a', 43539);
select 'a' ~ repeat('a', 43540);
select 'a' ~ repeat('(a)', 11874);
select 'a' ~ repeat('(a)', 11875);
select 'a' ~ (repeat('(', 1308) || 'a' || repeat(')*', 1308));
select 'a' ~ (repeat('(', 1309) || 'a' || repeat(')*', 1309));
select 'a' ~ (repeat('(?=', 5841) || 'a' || repeat(')', 5841));
select 'a' ~ (repeat('(?=', 5842) || 'a' || repeat(')', 5842));
select regexp_match('a', repeat('(', 7684) || 'a' || repeat(')', 7684));
select regexp_replace('a', repeat('a', 43540), 'x');
-- statement_timeout reaches the regex functions (CHECK_FOR_INTERRUPTS between matches,
-- INTERRUPT in the compiler and executor).
set statement_timeout = '50ms';
select length(regexp_replace(repeat('a', 5000000), 'a', 'b', 'g'));
select count(*) from regexp_matches(repeat('a', 2000000), 'a', 'g');
reset statement_timeout;
select length(regexp_replace(repeat('a', 50000), 'a', 'b', 'g'));
