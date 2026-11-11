-- The regex engine's character classification and case folding under the default collation
-- (builtin C.UTF-8: Unicode properties, POSIX-compatible digits and punctuation, the high
-- color map above U+07FF) and a C-ctype collation (ASCII only); flags, embedded options,
-- compile errors, jsonpath like_regex flag mapping, and some limits.
select regexp_matches('aé1٣Ⅷ_ ǅ€ ' collate "C", '[[:alpha:]]+', 'g');
select regexp_matches('aé1٣Ⅷ_ ǅ€ ', '[[:alpha:]]+', 'g');
select regexp_matches('aé1٣Ⅷ_ ǅ€ ', '[[:alnum:]]+', 'g');
select regexp_matches('aé1٣Ⅷ_ ǅ€ ', '[[:punct:]]+', 'g');
select regexp_matches('aé1٣Ⅷ_ ǅ€ ' collate "C", '[[:punct:]]+', 'g');
select regexp_matches('a٣1', '\d+', 'g'), regexp_matches('a٣1' collate "C", '\D+', 'g');
select 'ありがとう' ~ '^\w+$', 'ありがとう' collate "C" ~ '^\w+$', 'ありがとう' ~ '^[[:alpha:]]{5}$';
select 'ΣΑΣ' ~* 'σας', 'ΣΑΣ' collate "C" ~* 'σας', 'Straße' ~* 'STRASSE', 'É' ~* 'é', 'É' collate "C" ~* 'é';
select 'ǅ' ~* 'ǅ', 'ǆ' ~* 'ǅ', 'Ǆ' ~* 'ǅ', 'ǅ' ~* '[ǅ]', 'ǅ' ~* 'Ǆ';
select regexp_matches('x жж ЖЖ 漢字 ﾊﾝ', '[а-я]+', 'gi'), regexp_matches('x 一丁 漢字', '[一-龥]+', 'g');
select regexp_matches(E'a b c　d e', '\s', 'g');
select regexp_matches(E'a b c　d e' collate "C", '\S+', 'g');
select regexp_matches('A𝔄b𝔞C', '[[:upper:]]', 'g'), regexp_matches('A𝔄b𝔞C', '[[:lower:]]', 'gi');
select regexp_matches(E'͸x­​y', '[[:graph:]]', 'g');
select regexp_matches(E'͸x­​y \t', '[[:print:]]', 'g');
select regexp_matches(E'a\u0085b\u009fc', '[[:cntrl:]]', 'g');
select regexp_matches('añb', '[^[:ascii:]]'), regexp_matches('añb', '[[:blank:][:xdigit:]]+', 'g');
select regexp_matches('Ωmega ΩMEGA', '\mω\w+', 'gi'), regexp_matches('ωx ωy', '\yω', 'g');
select regexp_replace('ÄÖÜäöü', '[[:upper:]]', '_', 'g'), regexp_replace('ÄÖÜäöü' collate "C", '[[:upper:]]', '_', 'g');
select regexp_replace('ÄÖÜäöü', '[[:lower:]]', '_', 'gi');
select 'ﬀ' ~* 'FF', 'K' ~* 'k', 'k' ~* 'K', 'ſ' ~* 's', 'S' ~* 'ſ';
select 'a' ~ '[[:foo:]]', 'a' ~ '[[.foo.]]', 'a' ~ '[[=a=]]', 'A' ~* '[[=a=]]', '-' ~ '[[.hyphen.]]', 'a' ~ '[[.space.]]';
select regexp_matches('Ḁ ḁ ḀḀ', '(ḁ)\1', 'gi');
select regexp_matches('abcABC', '(?i)b'), regexp_matches('abcABC', '(?c)b', 'i'), regexp_matches('a.c', '***=.'), regexp_matches('ab', '***:(?i)B');
select regexp_matches('a+b', 'a+', 'e'), regexp_matches('a+b', '(?e)a+'), regexp_matches('a+b', 'a\+', 'b'), regexp_matches('aab', 'a\{2\}', 'b');
select regexp_matches(E'ab\ncd', '^\w', 'gn'), regexp_matches(E'ab\ncd', '.$', 'gw'), regexp_matches(E'ab\ncd', 'b.c', 'p'), regexp_matches(E'ab\ncd', 'b.c', 's');
select regexp_matches('a b', 'a b', 'x'), regexp_matches('a b', 'a\ b', 'x'), regexp_matches('ab', 'a # comment
b', 'x');
select regexp_matches('x', 'x', 'qx');
select regexp_matches('x', 'x', 'z');
select regexp_matches('é', 'é', 'é');
select 'x' ~ '(?é)x', 'x' collate "C" ~ '(?é)x';
select 'aaa' ~ 'a{256}', 'aaa' ~ 'a{1,256}', 'aaa' ~ 'a{3,2}', 'a' ~ 'a{', 'a{' ~ 'a{', 'a' ~ 'a{1';
select 'a' ~ '(', 'a' ~ ')', 'a' ~ '[', 'a' ~ ']', 'a' ~ '\', 'a' ~ '*', 'a' ~ 'a**', 'a' ~ '(?<a)', 'a' ~ '***?', 'a' ~ '***x';
select 'a' ~ '[b-a]', 'a' ~ '\1', 'a' ~ '(a)\2', 'a' ~ '\q', 'a' ~ '[\y]', 'a' ~ '(?z)a', 'a' ~ '(?ia';
select regexp_count('ababab', 'ab'), regexp_instr('xyzabc', 'b(c)', 1, 1, 1, '', 1), regexp_substr('abcabc', '(a)(x)?', 1, 2, '', 2), regexp_instr('abc','b',1,1,0,'',1);
select substring('foobar' from 'o(.)b'), substring('foobar' from 'x'), substring('foobar' from 'o(x)?b'), substring('foobar' similar '%#"o_b#"%' escape '#');
select 'abc' similar to 'a%', 'abc' similar to '(a|b)%c', 'abc' collate "C" similar to '[[:alpha:]]+';
select jsonb_path_query('["abc","ABC","a\nb"]', '$[*] ? (@ like_regex "^a" flag "i")'), jsonb_path_query('["a\nb"]', '$[*] ? (@ like_regex "a.b")'), jsonb_path_query('["a\nb"]', '$[*] ? (@ like_regex "a.b" flag "s")');
select jsonb_path_query('["a\nb"]', '$[*] ? (@ like_regex "^b" flag "m")'), jsonb_path_query('["a.b", "axb"]', '$[*] ? (@ like_regex "a.b" flag "q")');
select '$ ? (@ like_regex "a" flag "x")'::jsonpath;
select '$ ? (@ like_regex "(a" flag "i")'::jsonpath;
select '$ ? (@ like_regex "a(b" flag "q")'::jsonpath;
select regexp_split_to_array('aXbXc', 'x', 'i'), regexp_split_to_array('ẞaßb', 'ß', 'i');
select length(regexp_replace(repeat('abc ', 200000), '\w+', 'x', 'g'));
select count(*) from regexp_matches(repeat('foo bar baz ', 20000), '(\w)(\w*)', 'g');
select repeat('ab', 50000) ~ '(?<=a)b$', repeat('ab', 50000) ~ '^(ab)*\1$';
select 'x' ~ repeat('x*y*z*', 200), 'x' ~ repeat('(x)', 300);
select 'a' ~* '[\x01-\x7ffffffe]', 'a' ~* '[a-\U0010ffff]';
select regexp_matches(repeat('a', 30), '^(a+)+$');
