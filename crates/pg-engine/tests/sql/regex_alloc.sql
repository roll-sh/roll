-- Bounded repetitions so large that the lazy DFA's state-set bitmap would exceed MaxAllocSize
-- fail as Postgres's palloc does ("invalid memory alloc request size"), where smaller ones
-- run and the largest are "too complex" at compile time.
select 'x' ~ '(x{1,181}){1,181}';
select 'x' ~ '(x{1,182}){1,182}';
select 'x' ~ '(x{1,190}){1,190}';
select 'x' ~ '(x{1,200}){1,200}';
select 'x' ~ '(x{1,255}){1,180}';
select 'x' ~ '(x{1,255}){1,255}';
select 'x' ~ '((x{1,20}){1,20}){1,20}';
select substring('x' from '(x{1,200}){1,200}');
select regexp_match('x', '(x{1,200}){1,200}');
select 'x' ~ 'y(x{1,200}){1,200}';
select '' ~ '(x{1,200}){1,200}';
select 'x' ~ '(a|b|c|d|e|f|g|h|i|j|k|l|m|n|o|p|q){1,93}(x{1,255}){1,170}';
