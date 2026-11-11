-- Regular expression functions: Postgres ARE syntax and flags, match positions, empty
-- matches, replacement strings, SIMILAR TO translation and their errors.
select regexp_replace('abcabc', 'b', 'X'), regexp_replace('abcabc', 'b', 'X', 'g'), regexp_replace('abcabc', 'B', 'X', 'gi');
select regexp_replace('abcabcabc', 'b', 'X', 2), regexp_replace('abcabcabc', 'b', 'X', 1, 2), regexp_replace('abcabcabc', 'b', 'X', 1, 0), regexp_replace('abcabcabc', 'B', 'X', 3, 0, 'i');
select regexp_replace('abc', 'b', 'X', '1');
select regexp_replace('abc', 'b', 'X', 0);
select regexp_replace('abc', 'b', 'X', 1, -1);
select regexp_replace('abc', '', 'X', 'g'), regexp_replace('abc', 'x*', '-', 'g'), regexp_replace('', 'x*', '-', 'g');
select regexp_replace('abc', '(b)', '[\1\&\\\x$1]'), regexp_replace('abc', '(a)(b)?', '<\2\3>', 'g'), regexp_replace('a.b', '.', '\'), regexp_replace('abc','b','x\');
select regexp_replace('abc', 'b', 'X', 10), regexp_replace('abc', 'c', 'X', 3), regexp_replace('abc', '$', 'X', 4);
select regexp_match('foobarbequebaz', '(bar)(beque)'), regexp_match('abc', 'x'), regexp_match('abc', 'b'), regexp_match('abc', '(x)?b');
select regexp_match('abc', 'b', 'g');
select regexp_matches('foobarbequebazilbarfbonk', '(b[^b]+)(b[^b]+)', 'g');
select regexp_matches('abc', '', 'g');
select regexp_matches('abc', 'x*', 'g');
select regexp_matches('aaa', 'a*?', 'g');
select regexp_split_to_array('hello world', '\s+'), regexp_split_to_array('abc', ''), regexp_split_to_array('abc', 'x*'), regexp_split_to_array('', 'x'), regexp_split_to_array('abc','b*');
select regexp_split_to_array('a,b,,c', ','), regexp_split_to_array(',a,', ','), regexp_split_to_array('aXbxc', 'x', 'i');
select regexp_split_to_array('abc', 'b', 'g');
select regexp_split_to_table('the quick brown fox', '\s*');
select regexp_count('ABCABCAXYaxy', 'A.'), regexp_count('ABCABCAXYaxy', 'A.', 1, 'i'), regexp_count('ABCABCAXYaxy', 'A.', 4), regexp_count('abc', ''), regexp_count('abc', 'x*', 2), regexp_count('abc', 'b', 10);
select regexp_count('abc', 'b', 0);
select regexp_count('abc', 'b', 1, 'g');
select regexp_instr('abcdefghi', 'd.f'), regexp_instr('abcdefghi', 'd.f', 1, 1, 1), regexp_instr('ABCDEF','c(.)(..)', 1, 1, 0, 'i', 2), regexp_instr('ABCDEF','c(.)(..)', 1, 1, 0, 'i', 3), regexp_instr('abcabc','b',1,2), regexp_instr('abcabc','b',1,3), regexp_instr('abc','b',1,1,0,'',1), regexp_instr('abc','b',1,1,0,'',2), regexp_instr('abc','(x)?b',1,1,0,'',1);
select regexp_instr('abc', 'b', 1, 0);
select regexp_instr('abc', 'b', 1, 1, 2);
select regexp_instr('abc', 'b', 1, 1, 0, '', -1);
select regexp_substr('ABCDEF', 'c(.)(..)', 1, 1, 'i', 2), regexp_substr('abcabc','b.',1,2), regexp_substr('abc','x'), regexp_substr('abc','(x)?b',1,1,'',1), regexp_substr('abc','b',1,1,'',1), regexp_substr('abc','b',1,1,'',2);
select regexp_like('Hello', 'hello'), regexp_like('Hello', 'hello', 'i'), regexp_like('a
b', 'a.b'), regexp_like('a
b', 'a.b', 'n'), regexp_like('a
b', '^b', 'n'), regexp_like('a
b', '^b', 'p'), regexp_like('a
b', '^b', 'w'), regexp_like('a
b', 'a$', 'w'), regexp_like('a
b', 'a[^x]b', 'p');
select regexp_like('abc', 'a b c', 'x'), regexp_like('abc', 'a b # comment
 c', 'x'), regexp_like('a.c', 'a.c', 'q'), regexp_like('abc', 'a.c', 'q'), regexp_like('abc', '***=a.c'), regexp_like('a.c', '***=a.c'), regexp_like('ABC', '(?i)abc'), regexp_like('ABC', '***:(?i)abc');
select regexp_like('abc', 'a', 'z');
select regexp_like('abc', '(?z)abc');
select 'abc' ~ '\y', 'abc def' ~ '\mdef', 'abc def' ~ 'abc\M', 'abcdef' ~ '\Ydef', 'abc' ~ '[[:<:]]abc[[:>:]]', 'a1' ~ '\d', 'a' ~ '\D', ' ' ~ '\s', 'x' ~ '\S', '_' ~ '\w', '-' ~ '\W';
select 'a' ~ '\x61', 'a' ~ 'a', 'a' ~ '\U00000061', 'a' ~ '\141', E'\t' ~ '\t', E'\x07' ~ '\a', E'\x1b' ~ '\e', '\' ~ '\B', E'\x01' ~ '\cA', 'a{b' ~ 'a{b', 'aa' ~ '^a{2}$', 'a]' ~ '[]a]+', 'a-' ~ '[a-]{2}';
select 'a' ~ '\q';
select 'a' ~ '[a';
select 'a' ~ 'a)';
select 'a' ~ '(a';
select 'a' ~ '[b-a]';
select 'a' ~ '[[:foo:]]';
select 'a' ~ 'a{2';
select 'abc' ~ '*';
select 'AbC' ~* 'abc', 'AbC' ~* '[a-c]+$', 'x' ~* '[^a-z]', 'X' ~* '[^a-z]', 'Ab' !~* 'ab', 'Ab' !~ 'ab';
select 'a.b' similar to 'a.b', 'axb' similar to 'a.b', 'abc' similar to 'a%', 'abc' similar to 'a_c', 'abc' similar to '(a|x)bc', 'a$' similar to 'a$', 'a^' similar to 'a^', 'ab' similar to 'a[bc]', 'a]' similar to 'a[]]', 'a\b' similar to 'a\\b', 'a%' similar to 'a#%' escape '#', 'ab' similar to 'ab' escape '';
select 'x' similar to 'x' escape 'ab';
select similar_to_escape('a%b_c.d\e(f)[g]^$'), similar_to_escape('a#"b#"c', '#'), similar_to_escape('[]a]%[^]b]'), similar_to_escape('a\"b\"c');
select substring('foobar' similar '#"o_b#"%' escape '#'), substring('foobar' similar 'f#"o_b#"%' escape '#'), substring('abcdef' similar 'a#"%#"#"%' escape '#') ;
select substring('foobar' similar 'f#"o#"b#"%' escape '#');
select substring('foobar' from 'o(.)b'), substring('foobar' from 'o.b'), substring('foobar' from 'x'), substring('foobar' from '(x)?b');
select regexp_replace('foo bar', '\s', '_', 'g'), regexp_replace('aaa', 'a', 'b', 'g'), regexp_replace('Ab', '[a-z]', 'x', 'gi'), regexp_replace('ab'||chr(10)||'cd', '^', '>', 'gn');
select regexp_matches('ab'||chr(10)||'cd', '^.', 'gn'), regexp_matches('ab', '(a)|(b)', 'g');
select substring(x similar 'a#"%#"#"%' escape '#') from (values ('abc')) v(x);
select substring('abc' similar x escape '#') from (values ('a#"%#"#"%')) v(x);
select substring('abc' similar '(' escape '#');
select substring('abc' similar 'a' escape 'xx');
select 'abc' similar to 'a#"%#"#"%' escape '#';
