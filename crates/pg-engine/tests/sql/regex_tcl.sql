-- The Tcl regex test suite as Postgres carries it (src/test/modules/test_regex), run through
-- regexp_matches/regexp_replace: test_regex flags b/q become the b/q options, e becomes an
-- embedded (?e), i/n/p/w/x/g pass through; cases needing NOTBOL/NOTEOL/partial matching are
-- left out. The last cases are test_regex_utf8.sql's.
set standard_conforming_strings = on;
select regexp_matches('', '(*)', '');
select regexp_replace('', '(*)', '[\&]', '');
select regexp_matches('abc', 'abc', '');
select regexp_replace('abc', 'abc', '[\&]', '');
select regexp_matches('abc', 'abc', 'b');
select regexp_replace('abc', 'abc', '[\&]', 'b');
select regexp_matches('def', 'abc', '');
select regexp_replace('def', 'abc', '[\&]', '');
select regexp_matches('def', 'abc', 'b');
select regexp_replace('def', 'abc', '[\&]', 'b');
select regexp_matches('xyabxabce', 'abc', '');
select regexp_replace('xyabxabce', 'abc', '[\&]', '');
select regexp_matches('xyabxabce', 'abc', 'b');
select regexp_replace('xyabxabce', 'abc', '[\&]', 'b');
select regexp_matches('', '(?e)a', '');
select regexp_replace('', '(?e)a', '[\&]', '');
select regexp_matches('', 'a', 'q');
select regexp_replace('', 'a', '[\&]', 'q');
select regexp_matches('', 'a', 'xq');
select regexp_replace('', 'a', '[\&]', 'xq');
select regexp_matches('', 'a', 'nq');
select regexp_replace('', 'a', '[\&]', 'nq');
select regexp_matches('', 'a', 'b');
select regexp_replace('', 'a', '[\&]', 'b');
select regexp_matches('a', '', '');
select regexp_replace('a', '', '[\&]', '');
select regexp_matches('a', '', 'b');
select regexp_replace('a', '', '[\&]', 'b');
select regexp_matches('a', 'a|', '');
select regexp_replace('a', 'a|', '[\&]', '');
select regexp_matches('a', 'a|b', '');
select regexp_replace('a', 'a|b', '[\&]', '');
select regexp_matches('b', 'a|b', '');
select regexp_replace('b', 'a|b', '[\&]', '');
select regexp_matches('b', 'a||b', '');
select regexp_replace('b', 'a||b', '[\&]', '');
select regexp_matches('ab', 'ab', '');
select regexp_replace('ab', 'ab', '[\&]', '');
select regexp_matches('ab', 'ab', 'b');
select regexp_replace('ab', 'ab', '[\&]', 'b');
select regexp_matches('ae', '(a)e', '');
select regexp_replace('ae', '(a)e', '[\&]', '');
select regexp_matches('abeaae', '(.)\1e', '');
select regexp_replace('abeaae', '(.)\1e', '[\&]', '');
select regexp_matches('ab', '\(a\)b', 'b');
select regexp_replace('ab', '\(a\)b', '[\&]', 'b');
select regexp_matches('abc', 'a((b)c)', '');
select regexp_replace('abc', 'a((b)c)', '[\&]', '');
select regexp_matches('abc', 'a(b)(c)', '');
select regexp_replace('abc', 'a(b)(c)', '[\&]', '');
select regexp_matches('', 'a(b', '');
select regexp_replace('', 'a(b', '[\&]', '');
select regexp_matches('', 'a\(b', 'b');
select regexp_replace('', 'a\(b', '[\&]', 'b');
select regexp_matches('a)b', '(?e)a)b', '');
select regexp_replace('a)b', '(?e)a)b', '[\&]', '');
select regexp_matches('', 'a)b', '');
select regexp_replace('', 'a)b', '[\&]', '');
select regexp_matches('', 'a\)b', 'b');
select regexp_replace('', 'a\)b', '[\&]', 'b');
select regexp_matches('abc', 'a(?:b)c', '');
select regexp_replace('abc', 'a(?:b)c', '[\&]', '');
select regexp_matches('', '(?e)a(?:b)c', '');
select regexp_replace('', '(?e)a(?:b)c', '[\&]', '');
select regexp_matches('ab', 'a()b', '');
select regexp_replace('ab', 'a()b', '[\&]', '');
select regexp_matches('ab', 'a(?:)b', '');
select regexp_replace('ab', 'a(?:)b', '[\&]', '');
select regexp_matches('ac', 'a(|b)c', '');
select regexp_replace('ac', 'a(|b)c', '[\&]', '');
select regexp_matches('abc', 'a(b|)c', '');
select regexp_replace('abc', 'a(b|)c', '[\&]', '');
select regexp_matches('axb', 'a.b', '');
select regexp_replace('axb', 'a.b', '[\&]', '');
select regexp_matches('axb', 'a.b', 'b');
select regexp_replace('axb', 'a.b', '[\&]', 'b');
select regexp_matches(E'a\nb', 'a.b', 'n');
select regexp_replace(E'a\nb', 'a.b', '[\&]', 'n');
select regexp_matches(E'a\nb', 'a.b', 'nb');
select regexp_replace(E'a\nb', 'a.b', '[\&]', 'nb');
select regexp_matches('abd', 'a[bc]d', '');
select regexp_replace('abd', 'a[bc]d', '[\&]', '');
select regexp_matches('abd', 'a[bc]d', 'b');
select regexp_replace('abd', 'a[bc]d', '[\&]', 'b');
select regexp_matches('acd', 'a[bc]d', '');
select regexp_replace('acd', 'a[bc]d', '[\&]', '');
select regexp_matches('acd', 'a[bc]d', 'b');
select regexp_replace('acd', 'a[bc]d', '[\&]', 'b');
select regexp_matches('aed', 'a[bc]d', '');
select regexp_replace('aed', 'a[bc]d', '[\&]', '');
select regexp_matches('aed', 'a[bc]d', 'b');
select regexp_replace('aed', 'a[bc]d', '[\&]', 'b');
select regexp_matches('abd', 'a[^bc]d', '');
select regexp_replace('abd', 'a[^bc]d', '[\&]', '');
select regexp_matches('abd', 'a[^bc]d', 'b');
select regexp_replace('abd', 'a[^bc]d', '[\&]', 'b');
select regexp_matches('aed', 'a[^bc]d', '');
select regexp_replace('aed', 'a[^bc]d', '[\&]', '');
select regexp_matches('aed', 'a[^bc]d', 'b');
select regexp_replace('aed', 'a[^bc]d', '[\&]', 'b');
select regexp_matches(E'a\nd', 'a[^bc]d', 'p');
select regexp_replace(E'a\nd', 'a[^bc]d', '[\&]', 'p');
select regexp_matches(E'a\nd', 'a[^bc]d', 'pb');
select regexp_replace(E'a\nd', 'a[^bc]d', '[\&]', 'pb');
select regexp_matches('', '*', '');
select regexp_replace('', '*', '[\&]', '');
select regexp_matches('*', '*', 'b');
select regexp_replace('*', '*', '[\&]', 'b');
select regexp_matches('*', '\(*\)', 'b');
select regexp_replace('*', '\(*\)', '[\&]', 'b');
select regexp_matches('', '(*)', '');
select regexp_replace('', '(*)', '[\&]', '');
select regexp_matches('*', '^*', 'b');
select regexp_replace('*', '^*', '[\&]', 'b');
select regexp_matches('', '^*', '');
select regexp_replace('', '^*', '[\&]', '');
select regexp_matches('^b', '^b', '');
select regexp_replace('^b', '^b', '[\&]', '');
select regexp_matches('^b', '^b', 'b');
select regexp_replace('^b', '^b', '[\&]', 'b');
select regexp_matches('x^', 'x^', 'b');
select regexp_replace('x^', 'x^', '[\&]', 'b');
select regexp_matches('x', 'x^', '');
select regexp_replace('x', 'x^', '[\&]', '');
select regexp_matches(E'x\nb', E'\n^', 'n');
select regexp_replace(E'x\nb', E'\n^', '[\&]', 'n');
select regexp_matches('^b', '\(^b\)', 'b');
select regexp_replace('^b', '\(^b\)', '[\&]', 'b');
select regexp_matches('b', '(^b)', '');
select regexp_replace('b', '(^b)', '[\&]', '');
select regexp_matches('x', 'x$', '');
select regexp_replace('x', 'x$', '[\&]', '');
select regexp_matches('x', 'x$', 'b');
select regexp_replace('x', 'x$', '[\&]', 'b');
select regexp_matches('x', '\(x$\)', 'b');
select regexp_replace('x', '\(x$\)', '[\&]', 'b');
select regexp_matches('x', '(x$)', '');
select regexp_replace('x', '(x$)', '[\&]', '');
select regexp_matches('x$y', 'x$y', 'b');
select regexp_replace('x$y', 'x$y', '[\&]', 'b');
select regexp_matches('xy', 'x$y', '');
select regexp_replace('xy', 'x$y', '[\&]', '');
select regexp_matches(E'x\n', E'x$\n', 'n');
select regexp_replace(E'x\n', E'x$\n', '[\&]', 'n');
select regexp_matches('', '+', '');
select regexp_replace('', '+', '[\&]', '');
select regexp_matches('', '?', '');
select regexp_replace('', '?', '[\&]', '');
select regexp_matches('', 'x(\w)(?=(\1))', '');
select regexp_replace('', 'x(\w)(?=(\1))', '[\&]', '');
select regexp_matches('xfoo', 'x(?=((foo)))', '');
select regexp_replace('xfoo', 'x(?=((foo)))', '[\&]', '');
select regexp_matches('aa', 'a*', '');
select regexp_replace('aa', 'a*', '[\&]', '');
select regexp_matches('aa', 'a*', 'b');
select regexp_replace('aa', 'a*', '[\&]', 'b');
select regexp_matches('b', 'a*', '');
select regexp_replace('b', 'a*', '[\&]', '');
select regexp_matches('b', 'a*', 'b');
select regexp_replace('b', 'a*', '[\&]', 'b');
select regexp_matches('aa', 'a+', '');
select regexp_replace('aa', 'a+', '[\&]', '');
select regexp_matches('ab', 'a?b', '');
select regexp_replace('ab', 'a?b', '[\&]', '');
select regexp_matches('b', 'a?b', '');
select regexp_replace('b', 'a?b', '[\&]', '');
select regexp_matches('', '**', '');
select regexp_replace('', '**', '[\&]', '');
select regexp_matches('***', '**', 'b');
select regexp_replace('***', '**', '[\&]', 'b');
select regexp_matches('', 'a**', '');
select regexp_replace('', 'a**', '[\&]', '');
select regexp_matches('', 'a**', 'b');
select regexp_replace('', 'a**', '[\&]', 'b');
select regexp_matches('', 'a**b', '');
select regexp_replace('', 'a**b', '[\&]', '');
select regexp_matches('', 'a**b', 'b');
select regexp_replace('', 'a**b', '[\&]', 'b');
select regexp_matches('', '***', '');
select regexp_replace('', '***', '[\&]', '');
select regexp_matches('', '***', 'b');
select regexp_replace('', '***', '[\&]', 'b');
select regexp_matches('', 'a++', '');
select regexp_replace('', 'a++', '[\&]', '');
select regexp_matches('', 'a?+', '');
select regexp_replace('', 'a?+', '[\&]', '');
select regexp_matches('', 'a?*', '');
select regexp_replace('', 'a?*', '[\&]', '');
select regexp_matches('', 'a+*', '');
select regexp_replace('', 'a+*', '[\&]', '');
select regexp_matches('', 'a*+', '');
select regexp_replace('', 'a*+', '[\&]', '');
select regexp_matches('aaabbb', '.*b', 'b');
select regexp_replace('aaabbb', '.*b', '[\&]', 'b');
select regexp_matches('abcdef', '.\{1,10\}', 'b');
select regexp_replace('abcdef', '.\{1,10\}', '[\&]', 'b');
select regexp_matches('', 'a{0,1}', '');
select regexp_replace('', 'a{0,1}', '[\&]', '');
select regexp_matches('ac', 'a{0,1}', '');
select regexp_replace('ac', 'a{0,1}', '[\&]', '');
select regexp_matches('', 'a{1,0}', '');
select regexp_replace('', 'a{1,0}', '[\&]', '');
select regexp_matches('', 'a{1,2,3}', '');
select regexp_replace('', 'a{1,2,3}', '[\&]', '');
select regexp_matches('', 'a{257}', '');
select regexp_replace('', 'a{257}', '[\&]', '');
select regexp_matches('', 'a{1000}', '');
select regexp_replace('', 'a{1000}', '[\&]', '');
select regexp_matches('', 'a{1', '');
select regexp_replace('', 'a{1', '[\&]', '');
select regexp_matches('', 'a{1n}', '');
select regexp_replace('', 'a{1n}', '[\&]', '');
select regexp_matches('a{b', 'a{b', '');
select regexp_replace('a{b', 'a{b', '[\&]', '');
select regexp_matches('a{', 'a{', '');
select regexp_replace('a{', 'a{', '[\&]', '');
select regexp_matches('cb', 'a\{0,1\}b', 'b');
select regexp_replace('cb', 'a\{0,1\}b', '[\&]', 'b');
select regexp_matches('', 'a\{0,1', 'b');
select regexp_replace('', 'a\{0,1', '[\&]', 'b');
select regexp_matches('', 'a{0,1\', '');
select regexp_replace('', 'a{0,1\', '[\&]', '');
select regexp_matches('ab', 'a{0}b', '');
select regexp_replace('ab', 'a{0}b', '[\&]', '');
select regexp_matches('ab', 'a{0,0}b', '');
select regexp_replace('ab', 'a{0,0}b', '[\&]', '');
select regexp_matches('ab', 'a{0,1}b', '');
select regexp_replace('ab', 'a{0,1}b', '[\&]', '');
select regexp_matches('b', 'a{0,2}b', '');
select regexp_replace('b', 'a{0,2}b', '[\&]', '');
select regexp_matches('aab', 'a{0,2}b', '');
select regexp_replace('aab', 'a{0,2}b', '[\&]', '');
select regexp_matches('aab', 'a{0,}b', '');
select regexp_replace('aab', 'a{0,}b', '[\&]', '');
select regexp_matches('aab', 'a{1,1}b', '');
select regexp_replace('aab', 'a{1,1}b', '[\&]', '');
select regexp_matches('aaaab', 'a{1,3}b', '');
select regexp_replace('aaaab', 'a{1,3}b', '[\&]', '');
select regexp_matches('b', 'a{1,3}b', '');
select regexp_replace('b', 'a{1,3}b', '[\&]', '');
select regexp_matches('aab', 'a{1,}b', '');
select regexp_replace('aab', 'a{1,}b', '[\&]', '');
select regexp_matches('ab', 'a{2,3}b', '');
select regexp_replace('ab', 'a{2,3}b', '[\&]', '');
select regexp_matches('aaaab', 'a{2,3}b', '');
select regexp_replace('aaaab', 'a{2,3}b', '[\&]', '');
select regexp_matches('ab', 'a{2,}b', '');
select regexp_replace('ab', 'a{2,}b', '[\&]', '');
select regexp_matches('aaaab', 'a{2,}b', '');
select regexp_replace('aaaab', 'a{2,}b', '[\&]', '');
select regexp_matches('ac', 'a[bc]', '');
select regexp_replace('ac', 'a[bc]', '[\&]', '');
select regexp_matches('ac', 'a[bc]', 'b');
select regexp_replace('ac', 'a[bc]', '[\&]', 'b');
select regexp_matches('a-', 'a[-]', '');
select regexp_replace('a-', 'a[-]', '[\&]', '');
select regexp_matches('a-', 'a[-]', 'b');
select regexp_replace('a-', 'a[-]', '[\&]', 'b');
select regexp_matches('a-', 'a[[.-.]]', '');
select regexp_replace('a-', 'a[[.-.]]', '[\&]', '');
select regexp_matches('a-', 'a[[.-.]]', 'b');
select regexp_replace('a-', 'a[[.-.]]', '[\&]', 'b');
select regexp_matches('a0', 'a[[.zero.]]', '');
select regexp_replace('a0', 'a[[.zero.]]', '[\&]', '');
select regexp_matches('a0', 'a[[.zero.]]', 'b');
select regexp_replace('a0', 'a[[.zero.]]', '[\&]', 'b');
select regexp_matches('a2', 'a[[.zero.]-9]', '');
select regexp_replace('a2', 'a[[.zero.]-9]', '[\&]', '');
select regexp_matches('a2', 'a[[.zero.]-9]', 'b');
select regexp_replace('a2', 'a[[.zero.]-9]', '[\&]', 'b');
select regexp_matches('a2', 'a[0-[.9.]]', '');
select regexp_replace('a2', 'a[0-[.9.]]', '[\&]', '');
select regexp_matches('a2', 'a[0-[.9.]]', 'b');
select regexp_replace('a2', 'a[0-[.9.]]', '[\&]', 'b');
select regexp_matches('ay', 'a[[=Y=]]', 'i');
select regexp_replace('ay', 'a[[=Y=]]', '[\&]', 'i');
select regexp_matches('ay', 'a[[=Y=]]', 'ib');
select regexp_replace('ay', 'a[[=Y=]]', '[\&]', 'ib');
select regexp_matches('ay', 'a[[=Y=]]', '');
select regexp_replace('ay', 'a[[=Y=]]', '[\&]', '');
select regexp_matches('ay', 'a[[=Y=]]', 'b');
select regexp_replace('ay', 'a[[=Y=]]', '[\&]', 'b');
select regexp_matches('', 'a[0-[=x=]]', '');
select regexp_replace('', 'a[0-[=x=]]', '[\&]', '');
select regexp_matches('', 'a[0-[=x=]]', 'b');
select regexp_replace('', 'a[0-[=x=]]', '[\&]', 'b');
select regexp_matches('a0', 'a[[:digit:]]', '');
select regexp_replace('a0', 'a[[:digit:]]', '[\&]', '');
select regexp_matches('a0', 'a[[:digit:]]', 'b');
select regexp_replace('a0', 'a[[:digit:]]', '[\&]', 'b');
select regexp_matches('', 'a[[:woopsie:]]', '');
select regexp_replace('', 'a[[:woopsie:]]', '[\&]', '');
select regexp_matches('', 'a[[:woopsie:]]', 'b');
select regexp_replace('', 'a[[:woopsie:]]', '[\&]', 'b');
select regexp_matches('ab', 'a[[:digit:]]', '');
select regexp_replace('ab', 'a[[:digit:]]', '[\&]', '');
select regexp_matches('ab', 'a[[:digit:]]', 'b');
select regexp_replace('ab', 'a[[:digit:]]', '[\&]', 'b');
select regexp_matches('', 'a[0-[:digit:]]', '');
select regexp_replace('', 'a[0-[:digit:]]', '[\&]', '');
select regexp_matches('', 'a[0-[:digit:]]', 'b');
select regexp_replace('', 'a[0-[:digit:]]', '[\&]', 'b');
select regexp_matches('a', '[[:<:]]a', '');
select regexp_replace('a', '[[:<:]]a', '[\&]', '');
select regexp_matches('a', '[[:<:]]a', 'b');
select regexp_replace('a', '[[:<:]]a', '[\&]', 'b');
select regexp_matches('a', 'a[[:>:]]', '');
select regexp_replace('a', 'a[[:>:]]', '[\&]', '');
select regexp_matches('a', 'a[[:>:]]', 'b');
select regexp_replace('a', 'a[[:>:]]', '[\&]', 'b');
select regexp_matches('', 'a[[..]]b', '');
select regexp_replace('', 'a[[..]]b', '[\&]', '');
select regexp_matches('', 'a[[..]]b', 'b');
select regexp_replace('', 'a[[..]]b', '[\&]', 'b');
select regexp_matches('', 'a[[==]]b', '');
select regexp_replace('', 'a[[==]]b', '[\&]', '');
select regexp_matches('', 'a[[==]]b', 'b');
select regexp_replace('', 'a[[==]]b', '[\&]', 'b');
select regexp_matches('', 'a[[::]]b', '');
select regexp_replace('', 'a[[::]]b', '[\&]', '');
select regexp_matches('', 'a[[::]]b', 'b');
select regexp_replace('', 'a[[::]]b', '[\&]', 'b');
select regexp_matches('', 'a[[.a', '');
select regexp_replace('', 'a[[.a', '[\&]', '');
select regexp_matches('', 'a[[.a', 'b');
select regexp_replace('', 'a[[.a', '[\&]', 'b');
select regexp_matches('', 'a[[=a', '');
select regexp_replace('', 'a[[=a', '[\&]', '');
select regexp_matches('', 'a[[=a', 'b');
select regexp_replace('', 'a[[=a', '[\&]', 'b');
select regexp_matches('', 'a[[:a', '');
select regexp_replace('', 'a[[:a', '[\&]', '');
select regexp_matches('', 'a[[:a', 'b');
select regexp_replace('', 'a[[:a', '[\&]', 'b');
select regexp_matches('', 'a[', '');
select regexp_replace('', 'a[', '[\&]', '');
select regexp_matches('', 'a[', 'b');
select regexp_replace('', 'a[', '[\&]', 'b');
select regexp_matches('', 'a[b', '');
select regexp_replace('', 'a[b', '[\&]', '');
select regexp_matches('', 'a[b', 'b');
select regexp_replace('', 'a[b', '[\&]', 'b');
select regexp_matches('', 'a[b-', '');
select regexp_replace('', 'a[b-', '[\&]', '');
select regexp_matches('', 'a[b-', 'b');
select regexp_replace('', 'a[b-', '[\&]', 'b');
select regexp_matches('', 'a[b-c', '');
select regexp_replace('', 'a[b-c', '[\&]', '');
select regexp_matches('', 'a[b-c', 'b');
select regexp_replace('', 'a[b-c', '[\&]', 'b');
select regexp_matches('ab', 'a[b-c]', '');
select regexp_replace('ab', 'a[b-c]', '[\&]', '');
select regexp_matches('ab', 'a[b-c]', 'b');
select regexp_replace('ab', 'a[b-c]', '[\&]', 'b');
select regexp_matches('ab', 'a[b-b]', '');
select regexp_replace('ab', 'a[b-b]', '[\&]', '');
select regexp_matches('ab', 'a[b-b]', 'b');
select regexp_replace('ab', 'a[b-b]', '[\&]', 'b');
select regexp_matches('a2', 'a[1-2]', '');
select regexp_replace('a2', 'a[1-2]', '[\&]', '');
select regexp_matches('a2', 'a[1-2]', 'b');
select regexp_replace('a2', 'a[1-2]', '[\&]', 'b');
select regexp_matches('', 'a[c-b]', '');
select regexp_replace('', 'a[c-b]', '[\&]', '');
select regexp_matches('', 'a[c-b]', 'b');
select regexp_replace('', 'a[c-b]', '[\&]', 'b');
select regexp_matches('', 'a[a-b-c]', '');
select regexp_replace('', 'a[a-b-c]', '[\&]', '');
select regexp_matches('', 'a[a-b-c]', 'b');
select regexp_replace('', 'a[a-b-c]', '[\&]', 'b');
select regexp_matches('a?b', 'a[--?]b', '');
select regexp_replace('a?b', 'a[--?]b', '[\&]', '');
select regexp_matches('a?b', 'a[--?]b', 'b');
select regexp_replace('a?b', 'a[--?]b', '[\&]', 'b');
select regexp_matches('a-b', 'a[---]b', '');
select regexp_replace('a-b', 'a[---]b', '[\&]', '');
select regexp_matches('a-b', 'a[---]b', 'b');
select regexp_replace('a-b', 'a[---]b', '[\&]', 'b');
select regexp_matches('a]c', 'a[]b]c', '');
select regexp_replace('a]c', 'a[]b]c', '[\&]', '');
select regexp_matches('a]c', 'a[]b]c', 'b');
select regexp_replace('a]c', 'a[]b]c', '[\&]', 'b');
select regexp_matches('a]b', 'a[\]]b', '');
select regexp_replace('a]b', 'a[\]]b', '[\&]', '');
select regexp_matches('a]b', 'a[\]]b', 'b');
select regexp_replace('a]b', 'a[\]]b', '[\&]', 'b');
select regexp_matches('a\]b', 'a[\]]b', 'b');
select regexp_replace('a\]b', 'a[\]]b', '[\&]', 'b');
select regexp_matches('a\]b', '(?e)a[\]]b', '');
select regexp_replace('a\]b', '(?e)a[\]]b', '[\&]', '');
select regexp_matches('a\b', 'a[\\]b', '');
select regexp_replace('a\b', 'a[\\]b', '[\&]', '');
select regexp_matches('a\b', '(?e)a[\\]b', '');
select regexp_replace('a\b', '(?e)a[\\]b', '[\&]', '');
select regexp_matches('a\b', 'a[\\]b', 'b');
select regexp_replace('a\b', 'a[\\]b', '[\&]', 'b');
select regexp_matches('', 'a[\Z]b', '');
select regexp_replace('', 'a[\Z]b', '[\&]', '');
select regexp_matches('a[c', 'a[[b]c', '');
select regexp_replace('a[c', 'a[[b]c', '[\&]', '');
select regexp_matches('a[c', 'a[[b]c', 'b');
select regexp_replace('a[c', 'a[[b]c', '[\&]', 'b');
select regexp_matches('a', '^a', '');
select regexp_replace('a', '^a', '[\&]', '');
select regexp_matches('a', '^a', 'b');
select regexp_replace('a', '^a', '[\&]', 'b');
select regexp_matches('a', '^', '');
select regexp_replace('a', '^', '[\&]', '');
select regexp_matches('a', '^', 'b');
select regexp_replace('a', '^', '[\&]', 'b');
select regexp_matches('aba', 'a$', '');
select regexp_replace('aba', 'a$', '[\&]', '');
select regexp_matches('aba', 'a$', 'b');
select regexp_replace('aba', 'a$', '[\&]', 'b');
select regexp_matches('ab', '$', '');
select regexp_replace('ab', '$', '[\&]', '');
select regexp_matches('ab', '$', 'b');
select regexp_replace('ab', '$', '[\&]', 'b');
select regexp_matches('a', '^a', 'n');
select regexp_replace('a', '^a', '[\&]', 'n');
select regexp_matches('a', '^a', 'nb');
select regexp_replace('a', '^a', '[\&]', 'nb');
select regexp_matches(E'b\na', '^a', 'n');
select regexp_replace(E'b\na', '^a', '[\&]', 'n');
select regexp_matches(E'b\na', '^a', 'nb');
select regexp_replace(E'b\na', '^a', '[\&]', 'nb');
select regexp_matches(E'a\na', '^a', 'w');
select regexp_replace(E'a\na', '^a', '[\&]', 'w');
select regexp_matches(E'a\na', '^a', 'wb');
select regexp_replace(E'a\na', '^a', '[\&]', 'wb');
select regexp_matches('a', 'a$', 'n');
select regexp_replace('a', 'a$', '[\&]', 'n');
select regexp_matches('a', 'a$', 'nb');
select regexp_replace('a', 'a$', '[\&]', 'nb');
select regexp_matches(E'a\nb', 'a$', 'n');
select regexp_replace(E'a\nb', 'a$', '[\&]', 'n');
select regexp_matches(E'a\nb', 'a$', 'nb');
select regexp_replace(E'a\nb', 'a$', '[\&]', 'nb');
select regexp_matches(E'a\na', 'a$', 'n');
select regexp_replace(E'a\na', 'a$', '[\&]', 'n');
select regexp_matches(E'a\na', 'a$', 'nb');
select regexp_replace(E'a\na', 'a$', '[\&]', 'nb');
select regexp_matches('a', '^^', '');
select regexp_replace('a', '^^', '[\&]', '');
select regexp_matches('^', '^^', 'b');
select regexp_replace('^', '^^', '[\&]', 'b');
select regexp_matches('a', '$$', '');
select regexp_replace('a', '$$', '[\&]', '');
select regexp_matches('$', '$$', 'b');
select regexp_replace('$', '$$', '[\&]', 'b');
select regexp_matches('', '^$', '');
select regexp_replace('', '^$', '[\&]', '');
select regexp_matches('', '^$', 'b');
select regexp_replace('', '^$', '[\&]', 'b');
select regexp_matches('a', '^$', '');
select regexp_replace('a', '^$', '[\&]', '');
select regexp_matches('a', '^$', 'b');
select regexp_replace('a', '^$', '[\&]', 'b');
select regexp_matches(E'a\n\nb', '^$', 'n');
select regexp_replace(E'a\n\nb', '^$', '[\&]', 'n');
select regexp_matches(E'a\n\nb', '^$', 'nb');
select regexp_replace(E'a\n\nb', '^$', '[\&]', 'nb');
select regexp_matches('', '$^', '');
select regexp_replace('', '$^', '[\&]', '');
select regexp_matches('$^', '$^', 'b');
select regexp_replace('$^', '$^', '[\&]', 'b');
select regexp_matches('a', '\Aa', '');
select regexp_replace('a', '\Aa', '[\&]', '');
select regexp_matches('a', 'a\Z', '');
select regexp_replace('a', 'a\Z', '[\&]', '');
select regexp_matches('', '^*', '');
select regexp_replace('', '^*', '[\&]', '');
select regexp_matches('', '$*', '');
select regexp_replace('', '$*', '[\&]', '');
select regexp_matches('', '\A*', '');
select regexp_replace('', '\A*', '[\&]', '');
select regexp_matches('', '\Z*', '');
select regexp_replace('', '\Z*', '[\&]', '');
select regexp_matches('a', '[[:<:]]a', '');
select regexp_replace('a', '[[:<:]]a', '[\&]', '');
select regexp_matches('a', '[[:<:]]a', 'b');
select regexp_replace('a', '[[:<:]]a', '[\&]', 'b');
select regexp_matches('-a', '[[:<:]]a', '');
select regexp_replace('-a', '[[:<:]]a', '[\&]', '');
select regexp_matches('-a', '[[:<:]]a', 'b');
select regexp_replace('-a', '[[:<:]]a', '[\&]', 'b');
select regexp_matches('ba', '[[:<:]]a', '');
select regexp_replace('ba', '[[:<:]]a', '[\&]', '');
select regexp_matches('ba', '[[:<:]]a', 'b');
select regexp_replace('ba', '[[:<:]]a', '[\&]', 'b');
select regexp_matches('a', 'a[[:>:]]', '');
select regexp_replace('a', 'a[[:>:]]', '[\&]', '');
select regexp_matches('a', 'a[[:>:]]', 'b');
select regexp_replace('a', 'a[[:>:]]', '[\&]', 'b');
select regexp_matches('a-', 'a[[:>:]]', '');
select regexp_replace('a-', 'a[[:>:]]', '[\&]', '');
select regexp_matches('a-', 'a[[:>:]]', 'b');
select regexp_replace('a-', 'a[[:>:]]', '[\&]', 'b');
select regexp_matches('ab', 'a[[:>:]]', '');
select regexp_replace('ab', 'a[[:>:]]', '[\&]', '');
select regexp_matches('ab', 'a[[:>:]]', 'b');
select regexp_replace('ab', 'a[[:>:]]', '[\&]', 'b');
select regexp_matches('a', '\<a', 'b');
select regexp_replace('a', '\<a', '[\&]', 'b');
select regexp_matches('ba', '\<a', 'b');
select regexp_replace('ba', '\<a', '[\&]', 'b');
select regexp_matches('a', 'a\>', 'b');
select regexp_replace('a', 'a\>', '[\&]', 'b');
select regexp_matches('ab', 'a\>', 'b');
select regexp_replace('ab', 'a\>', '[\&]', 'b');
select regexp_matches('a', '\ya', '');
select regexp_replace('a', '\ya', '[\&]', '');
select regexp_matches('ba', '\ya', '');
select regexp_replace('ba', '\ya', '[\&]', '');
select regexp_matches('a', 'a\y', '');
select regexp_replace('a', 'a\y', '[\&]', '');
select regexp_matches('ab', 'a\y', '');
select regexp_replace('ab', 'a\y', '[\&]', '');
select regexp_matches('ab', 'a\Y', '');
select regexp_replace('ab', 'a\Y', '[\&]', '');
select regexp_matches('a-', 'a\Y', '');
select regexp_replace('a-', 'a\Y', '[\&]', '');
select regexp_matches('a', 'a\Y', '');
select regexp_replace('a', 'a\Y', '[\&]', '');
select regexp_matches('-a', '-\Y', '');
select regexp_replace('-a', '-\Y', '[\&]', '');
select regexp_matches('-%', '-\Y', '');
select regexp_replace('-%', '-\Y', '[\&]', '');
select regexp_matches('a-', '\Y-', '');
select regexp_replace('a-', '\Y-', '[\&]', '');
select regexp_matches('', '[[:<:]]*', '');
select regexp_replace('', '[[:<:]]*', '[\&]', '');
select regexp_matches('', '[[:>:]]*', '');
select regexp_replace('', '[[:>:]]*', '[\&]', '');
select regexp_matches('', '\<*', 'b');
select regexp_replace('', '\<*', '[\&]', 'b');
select regexp_matches('', '\>*', 'b');
select regexp_replace('', '\>*', '[\&]', 'b');
select regexp_matches('', '\y*', '');
select regexp_replace('', '\y*', '[\&]', '');
select regexp_matches('', '\Y*', '');
select regexp_replace('', '\Y*', '[\&]', '');
select regexp_matches('a', '\ma', '');
select regexp_replace('a', '\ma', '[\&]', '');
select regexp_matches('ba', '\ma', '');
select regexp_replace('ba', '\ma', '[\&]', '');
select regexp_matches('a', 'a\M', '');
select regexp_replace('a', 'a\M', '[\&]', '');
select regexp_matches('ab', 'a\M', '');
select regexp_replace('ab', 'a\M', '[\&]', '');
select regexp_matches('a', '\Ma', '');
select regexp_replace('a', '\Ma', '[\&]', '');
select regexp_matches('a', 'a\m', '');
select regexp_replace('a', 'a\m', '[\&]', '');
select regexp_matches('a0b', 'a\db', '');
select regexp_replace('a0b', 'a\db', '[\&]', '');
select regexp_matches('axb', 'a\db', '');
select regexp_replace('axb', 'a\db', '[\&]', '');
select regexp_matches('a0b', 'a\Db', '');
select regexp_replace('a0b', 'a\Db', '[\&]', '');
select regexp_matches('axb', 'a\Db', '');
select regexp_replace('axb', 'a\Db', '[\&]', '');
select regexp_matches('a b', 'a\sb', '');
select regexp_replace('a b', 'a\sb', '[\&]', '');
select regexp_matches(E'a\tb', 'a\sb', '');
select regexp_replace(E'a\tb', 'a\sb', '[\&]', '');
select regexp_matches(E'a\nb', 'a\sb', '');
select regexp_replace(E'a\nb', 'a\sb', '[\&]', '');
select regexp_matches('axb', 'a\sb', '');
select regexp_replace('axb', 'a\sb', '[\&]', '');
select regexp_matches('axb', 'a\Sb', '');
select regexp_replace('axb', 'a\Sb', '[\&]', '');
select regexp_matches('a b', 'a\Sb', '');
select regexp_replace('a b', 'a\Sb', '[\&]', '');
select regexp_matches('axb', 'a\wb', '');
select regexp_replace('axb', 'a\wb', '[\&]', '');
select regexp_matches('a-b', 'a\wb', '');
select regexp_replace('a-b', 'a\wb', '[\&]', '');
select regexp_matches('axb', 'a\Wb', '');
select regexp_replace('axb', 'a\Wb', '[\&]', '');
select regexp_matches('a-b', 'a\Wb', '');
select regexp_replace('a-b', 'a\Wb', '[\&]', '');
select regexp_matches('adze-guz', '\y\w+z\y', '');
select regexp_replace('adze-guz', '\y\w+z\y', '[\&]', '');
select regexp_matches('a1b', 'a[\d]b', '');
select regexp_replace('a1b', 'a[\d]b', '[\&]', '');
select regexp_matches('a b', 'a[\s]b', '');
select regexp_replace('a b', 'a[\s]b', '[\&]', '');
select regexp_matches('axb', 'a[\w]b', '');
select regexp_replace('axb', 'a[\w]b', '[\&]', '');
select regexp_matches('ab01_~-`**', '[\w-~]*', '');
select regexp_replace('ab01_~-`**', '[\w-~]*', '[\&]', '');
select regexp_matches('ab01_~-`**', '[~-\w]*', '');
select regexp_replace('ab01_~-`**', '[~-\w]*', '[\&]', '');
select regexp_matches('ab01~-`**', '[[:alnum:]-~]*', '');
select regexp_replace('ab01~-`**', '[[:alnum:]-~]*', '[\&]', '');
select regexp_matches('ab01~-`**', '[~-[:alnum:]]*', '');
select regexp_replace('ab01~-`**', '[~-[:alnum:]]*', '[\&]', '');
select regexp_matches('0123456789abc*', '[\D]', '');
select regexp_replace('0123456789abc*', '[\D]', '[\&]', '');
select regexp_matches('abc0123456789*', '[^\D]', '');
select regexp_replace('abc0123456789*', '[^\D]', '[\&]', '');
select regexp_matches('0123456789abc*', '[1\D7]', '');
select regexp_replace('0123456789abc*', '[1\D7]', '[\&]', '');
select regexp_matches('0123456789abc*', '[7\D1]', '');
select regexp_replace('0123456789abc*', '[7\D1]', '[\&]', '');
select regexp_matches('abc0123456789*', '[^0\D1]', '');
select regexp_replace('abc0123456789*', '[^0\D1]', '[\&]', '');
select regexp_matches('abc0123456789*', '[^1\D0]', '');
select regexp_replace('abc0123456789*', '[^1\D0]', '[\&]', '');
select regexp_matches('0123456789abc_*', '\W', '');
select regexp_replace('0123456789abc_*', '\W', '[\&]', '');
select regexp_matches('0123456789abc_*', '[\W]', '');
select regexp_replace('0123456789abc_*', '[\W]', '[\&]', '');
select regexp_matches('012  3456789abc_*', '[\s\S]*', '');
select regexp_replace('012  3456789abc_*', '[\s\S]*', '[\&]', '');
select regexp_matches('0123456789abc*', '(?:[^\d\D]){0}', '');
select regexp_replace('0123456789abc*', '(?:[^\d\D]){0}', '[\&]', '');
select regexp_matches('0123456789abc*', '[^\d\D]', '');
select regexp_replace('0123456789abc*', '[^\d\D]', '[\&]', '');
select regexp_matches(E'abc  \n  def', '\s+', '');
select regexp_replace(E'abc  \n  def', '\s+', '[\&]', '');
select regexp_matches(E'abc  \n  def', '\s+', 'n');
select regexp_replace(E'abc  \n  def', '\s+', '[\&]', 'n');
select regexp_matches(E'abc  \n  def', '[\s]+', '');
select regexp_replace(E'abc  \n  def', '[\s]+', '[\&]', '');
select regexp_matches(E'abc  \n  def', '[\s]+', 'n');
select regexp_replace(E'abc  \n  def', '[\s]+', '[\&]', 'n');
select regexp_matches(E'abc\ndef', '\S+', '');
select regexp_replace(E'abc\ndef', '\S+', '[\&]', '');
select regexp_matches(E'abc\ndef', '\S+', 'n');
select regexp_replace(E'abc\ndef', '\S+', '[\&]', 'n');
select regexp_matches(E'abc\ndef', '[\S]+', '');
select regexp_replace(E'abc\ndef', '[\S]+', '[\&]', '');
select regexp_matches(E'abc\ndef', '[\S]+', 'n');
select regexp_replace(E'abc\ndef', '[\S]+', '[\&]', 'n');
select regexp_matches(E'012\n345', '\d+', '');
select regexp_replace(E'012\n345', '\d+', '[\&]', '');
select regexp_matches(E'012\n345', '\d+', 'n');
select regexp_replace(E'012\n345', '\d+', '[\&]', 'n');
select regexp_matches(E'012\n345', '[\d]+', '');
select regexp_replace(E'012\n345', '[\d]+', '[\&]', '');
select regexp_matches(E'012\n345', '[\d]+', 'n');
select regexp_replace(E'012\n345', '[\d]+', '[\&]', 'n');
select regexp_matches(E'abc\ndef345', '\D+', '');
select regexp_replace(E'abc\ndef345', '\D+', '[\&]', '');
select regexp_matches(E'abc\ndef345', '\D+', 'n');
select regexp_replace(E'abc\ndef345', '\D+', '[\&]', 'n');
select regexp_matches(E'abc\ndef345', '[\D]+', '');
select regexp_replace(E'abc\ndef345', '[\D]+', '[\&]', '');
select regexp_matches(E'abc\ndef345', '[\D]+', 'n');
select regexp_replace(E'abc\ndef345', '[\D]+', '[\&]', 'n');
select regexp_matches(E'abc_012\ndef', '\w+', '');
select regexp_replace(E'abc_012\ndef', '\w+', '[\&]', '');
select regexp_matches(E'abc_012\ndef', '\w+', 'n');
select regexp_replace(E'abc_012\ndef', '\w+', '[\&]', 'n');
select regexp_matches(E'abc_012\ndef', '[\w]+', '');
select regexp_replace(E'abc_012\ndef', '[\w]+', '[\&]', '');
select regexp_matches(E'abc_012\ndef', '[\w]+', 'n');
select regexp_replace(E'abc_012\ndef', '[\w]+', '[\&]', 'n');
select regexp_matches(E'***\n@@@___', '\W+', '');
select regexp_replace(E'***\n@@@___', '\W+', '[\&]', '');
select regexp_matches(E'***\n@@@___', '\W+', 'n');
select regexp_replace(E'***\n@@@___', '\W+', '[\&]', 'n');
select regexp_matches(E'***\n@@@___', '[\W]+', '');
select regexp_replace(E'***\n@@@___', '[\W]+', '[\&]', '');
select regexp_matches(E'***\n@@@___', '[\W]+', 'n');
select regexp_replace(E'***\n@@@___', '[\W]+', '[\&]', 'n');
select regexp_matches('', 'a\', '');
select regexp_replace('', 'a\', '[\&]', '');
select regexp_matches('', 'a\', 'b');
select regexp_replace('', 'a\', '[\&]', 'b');
select regexp_matches('a<b', 'a\<b', '');
select regexp_replace('a<b', 'a\<b', '[\&]', '');
select regexp_matches('a<b', '(?e)a\<b', '');
select regexp_replace('a<b', '(?e)a\<b', '[\&]', '');
select regexp_matches('awb', 'a\wb', 'b');
select regexp_replace('awb', 'a\wb', '[\&]', 'b');
select regexp_matches('awb', '(?e)a\wb', '');
select regexp_replace('awb', '(?e)a\wb', '[\&]', '');
select regexp_matches(E'a\007b', 'a\ab', '');
select regexp_replace(E'a\007b', 'a\ab', '[\&]', '');
select regexp_matches(E'a\bb', 'a\bb', '');
select regexp_replace(E'a\bb', 'a\bb', '[\&]', '');
select regexp_matches('a\b', 'a\Bb', '');
select regexp_replace('a\b', 'a\Bb', '[\&]', '');
select regexp_matches(E'a\bb', 'a\chb', '');
select regexp_replace(E'a\bb', 'a\chb', '[\&]', '');
select regexp_matches(E'a\bb', 'a\cHb', '');
select regexp_replace(E'a\bb', 'a\cHb', '[\&]', '');
select regexp_matches(E'a\033', 'a\e', '');
select regexp_replace(E'a\033', 'a\e', '[\&]', '');
select regexp_matches(E'a\fb', 'a\fb', '');
select regexp_replace(E'a\fb', 'a\fb', '[\&]', '');
select regexp_matches(E'a\nb', 'a\nb', '');
select regexp_replace(E'a\nb', 'a\nb', '[\&]', '');
select regexp_matches(E'a\rb', 'a\rb', '');
select regexp_replace(E'a\rb', 'a\rb', '[\&]', '');
select regexp_matches(E'a\tb', 'a\tb', '');
select regexp_replace(E'a\tb', 'a\tb', '[\&]', '');
select regexp_matches(E'a\bx', 'a\u0008x', '');
select regexp_replace(E'a\bx', 'a\u0008x', '[\&]', '');
select regexp_matches(E'a\bx', 'a\u008x', '');
select regexp_replace(E'a\bx', 'a\u008x', '[\&]', '');
select regexp_matches(E'a\b8x', 'a\u00088x', '');
select regexp_replace(E'a\b8x', 'a\u00088x', '[\&]', '');
select regexp_matches(E'a\bx', 'a\U00000008x', '');
select regexp_replace(E'a\bx', 'a\U00000008x', '[\&]', '');
select regexp_matches(E'a\bx', 'a\U0000008x', '');
select regexp_replace(E'a\bx', 'a\U0000008x', '[\&]', '');
select regexp_matches(E'a\013b', 'a\vb', '');
select regexp_replace(E'a\013b', 'a\vb', '[\&]', '');
select regexp_matches(E'a\bx', 'a\x08x', '');
select regexp_replace(E'a\bx', 'a\x08x', '[\&]', '');
select regexp_matches('', 'a\xq', '');
select regexp_replace('', 'a\xq', '[\&]', '');
select regexp_matches(E'a\bx', 'a\x08x', '');
select regexp_replace(E'a\bx', 'a\x08x', '[\&]', '');
select regexp_matches('', 'a\z', '');
select regexp_replace('', 'a\z', '[\&]', '');
select regexp_matches(E'a\bb', 'a\010b', '');
select regexp_replace(E'a\bb', 'a\010b', '[\&]', '');
select regexp_matches('abbcbb', 'a(b*)c\1', '');
select regexp_replace('abbcbb', 'a(b*)c\1', '[\&]', '');
select regexp_matches('ac', 'a(b*)c\1', '');
select regexp_replace('ac', 'a(b*)c\1', '[\&]', '');
select regexp_matches('abbcb', 'a(b*)c\1', '');
select regexp_replace('abbcb', 'a(b*)c\1', '[\&]', '');
select regexp_matches('abbcbb', 'a(b*)\1', '');
select regexp_replace('abbcbb', 'a(b*)\1', '[\&]', '');
select regexp_matches('abbcbb', 'a(b|bb)\1', '');
select regexp_replace('abbcbb', 'a(b|bb)\1', '[\&]', '');
select regexp_matches('abb', 'a([bc])\1', '');
select regexp_replace('abb', 'a([bc])\1', '[\&]', '');
select regexp_matches('abc', 'a([bc])\1', '');
select regexp_replace('abc', 'a([bc])\1', '[\&]', '');
select regexp_matches('abcabb', 'a([bc])\1', '');
select regexp_replace('abcabb', 'a([bc])\1', '[\&]', '');
select regexp_matches('abc', 'a([bc])*\1', '');
select regexp_replace('abc', 'a([bc])*\1', '[\&]', '');
select regexp_matches('abB', 'a([bc])\1', '');
select regexp_replace('abB', 'a([bc])\1', '[\&]', '');
select regexp_matches('abB', 'a([bc])\1', 'i');
select regexp_replace('abB', 'a([bc])\1', '[\&]', 'i');
select regexp_matches('abbb', 'a([bc])\1+', '');
select regexp_replace('abbb', 'a([bc])\1+', '[\&]', '');
select regexp_matches('abbbb', 'a([bc])\1{3,4}', '');
select regexp_replace('abbbb', 'a([bc])\1{3,4}', '[\&]', '');
select regexp_matches('abbb', 'a([bc])\1{3,4}', '');
select regexp_replace('abbb', 'a([bc])\1{3,4}', '[\&]', '');
select regexp_matches('abbb', 'a([bc])\1*', '');
select regexp_replace('abbb', 'a([bc])\1*', '[\&]', '');
select regexp_matches('ab', 'a([bc])\1*', '');
select regexp_replace('ab', 'a([bc])\1*', '[\&]', '');
select regexp_matches('ab', 'a([bc])(\1*)', '');
select regexp_replace('ab', 'a([bc])(\1*)', '[\&]', '');
select regexp_matches('', 'a((b)\1)', '');
select regexp_replace('', 'a((b)\1)', '[\&]', '');
select regexp_matches('', 'a(b)c\2', '');
select regexp_replace('', 'a(b)c\2', '[\&]', '');
select regexp_matches('abbcbb', 'a\(b*\)c\1', 'b');
select regexp_replace('abbcbb', 'a\(b*\)c\1', '[\&]', 'b');
select regexp_matches('bbb', '^([bc])\1*$', '');
select regexp_replace('bbb', '^([bc])\1*$', '[\&]', '');
select regexp_matches('ccc', '^([bc])\1*$', '');
select regexp_replace('ccc', '^([bc])\1*$', '[\&]', '');
select regexp_matches('bcb', '^([bc])\1*$', '');
select regexp_replace('bcb', '^([bc])\1*$', '[\&]', '');
select regexp_matches('abc abc abc', '^(\w+)( \1)+$', '');
select regexp_replace('abc abc abc', '^(\w+)( \1)+$', '[\&]', '');
select regexp_matches('abc abd abc', '^(\w+)( \1)+$', '');
select regexp_replace('abc abd abc', '^(\w+)( \1)+$', '[\&]', '');
select regexp_matches('abc abc abd', '^(\w+)( \1)+$', '');
select regexp_replace('abc abc abd', '^(\w+)( \1)+$', '[\&]', '');
select regexp_matches('abc abc abc', '^(.+)( \1)+$', '');
select regexp_replace('abc abc abc', '^(.+)( \1)+$', '[\&]', '');
select regexp_matches('abc abd abc', '^(.+)( \1)+$', '');
select regexp_replace('abc abd abc', '^(.+)( \1)+$', '[\&]', '');
select regexp_matches('abc abc abd', '^(.+)( \1)+$', '');
select regexp_replace('abc abc abd', '^(.+)( \1)+$', '[\&]', '');
select regexp_matches('abcdef', '^(.)\1|\1.', '');
select regexp_replace('abcdef', '^(.)\1|\1.', '[\&]', '');
select regexp_matches('abadef', '^((.)\2|..)\2', '');
select regexp_replace('abadef', '^((.)\2|..)\2', '[\&]', '');
select regexp_matches('abc abc abc', '(^\w+).*\1', '');
select regexp_replace('abc abc abc', '(^\w+).*\1', '[\&]', '');
select regexp_matches('abc abcd abd', '(^\w+\M).*\1', '');
select regexp_replace('abc abcd abd', '(^\w+\M).*\1', '[\&]', '');
select regexp_matches('abc abcd abd', '(\w+(?= )).*\1', '');
select regexp_replace('abc abcd abd', '(\w+(?= )).*\1', '[\&]', '');
select regexp_matches(E'a\bb', 'a\010b', '');
select regexp_replace(E'a\bb', 'a\010b', '[\&]', '');
select regexp_matches(E'a\0070b', 'a\0070b', '');
select regexp_replace(E'a\0070b', 'a\0070b', '[\&]', '');
select regexp_matches(E'a\007b', 'a\07b', '');
select regexp_replace(E'a\007b', 'a\07b', '[\&]', '');
select regexp_matches(E'abbbbbbbbbb\007c', 'a(b)(b)(b)(b)(b)(b)(b)(b)(b)(b)\07c', '');
select regexp_replace(E'abbbbbbbbbb\007c', 'a(b)(b)(b)(b)(b)(b)(b)(b)(b)(b)\07c', '[\&]', '');
select regexp_matches('', 'a\7b', '');
select regexp_replace('', 'a\7b', '[\&]', '');
select regexp_matches(E'a\bb', 'a\10b', '');
select regexp_replace(E'a\bb', 'a\10b', '[\&]', '');
select regexp_matches('aAb', 'a\101b', '');
select regexp_replace('aAb', 'a\101b', '[\&]', '');
select regexp_matches('abbbbbbbbbbbc', 'a(b)(b)(b)(b)(b)(b)(b)(b)(b)(b)\10c', '');
select regexp_replace('abbbbbbbbbbbc', 'a(b)(b)(b)(b)(b)(b)(b)(b)(b)(b)\10c', '[\&]', '');
select regexp_matches('', 'a((((((((((b\10))))))))))c', '');
select regexp_replace('', 'a((((((((((b\10))))))))))c', '[\&]', '');
select regexp_matches(E'a\nb', 'a\12b', '');
select regexp_replace(E'a\nb', 'a\12b', '[\&]', '');
select regexp_matches('', 'a\12b', 'b');
select regexp_replace('', 'a\12b', '[\&]', 'b');
select regexp_matches('a12b', '(?e)a\12b', '');
select regexp_replace('a12b', '(?e)a\12b', '[\&]', '');
select regexp_matches('a81b', 'a\701b', '');
select regexp_replace('a81b', 'a\701b', '[\&]', '');
select regexp_matches('abc', 'a b c', 'x');
select regexp_replace('abc', 'a b c', '[\&]', 'x');
select regexp_matches('abcd', E'a b #oops\nc\td', 'x');
select regexp_replace('abcd', E'a b #oops\nc\td', '[\&]', 'x');
select regexp_matches(E'a b\tc', E'a\\ b\\\tc', 'x');
select regexp_replace(E'a b\tc', E'a\\ b\\\tc', '[\&]', 'x');
select regexp_matches('ab#c', 'a b\#c', 'x');
select regexp_replace('ab#c', 'a b\#c', '[\&]', 'x');
select regexp_matches('ab e', 'a b[c d]e', 'x');
select regexp_replace('ab e', 'a b[c d]e', '[\&]', 'x');
select regexp_matches('ab#e', 'a b[c#d]e', 'x');
select regexp_replace('ab#e', 'a b[c#d]e', '[\&]', 'x');
select regexp_matches('abde', 'a b[c#d]e', 'x');
select regexp_replace('abde', 'a b[c#d]e', '[\&]', 'x');
select regexp_matches('ab{d', 'ab{ d', 'x');
select regexp_replace('ab{d', 'ab{ d', '[\&]', 'x');
select regexp_matches('abc', 'ab{ 1 , 2 }c', 'x');
select regexp_replace('abc', 'ab{ 1 , 2 }c', '[\&]', 'x');
select regexp_matches('ab', 'a(?#comment)b', '');
select regexp_replace('ab', 'a(?#comment)b', '[\&]', '');
select regexp_matches('ab', 'a^b', '');
select regexp_replace('ab', 'a^b', '[\&]', '');
select regexp_matches('Ab', 'ab', 'i');
select regexp_replace('Ab', 'ab', '[\&]', 'i');
select regexp_matches('Ab', 'ab', 'ib');
select regexp_replace('Ab', 'ab', '[\&]', 'ib');
select regexp_matches('aC', 'a[bc]', 'i');
select regexp_replace('aC', 'a[bc]', '[\&]', 'i');
select regexp_matches('aC', 'a[bc]', 'ib');
select regexp_replace('aC', 'a[bc]', '[\&]', 'ib');
select regexp_matches('aB', 'a[^bc]', 'i');
select regexp_replace('aB', 'a[^bc]', '[\&]', 'i');
select regexp_matches('aB', 'a[^bc]', 'ib');
select regexp_replace('aB', 'a[^bc]', '[\&]', 'ib');
select regexp_matches('aC', 'a[b-d]', 'i');
select regexp_replace('aC', 'a[b-d]', '[\&]', 'i');
select regexp_matches('aC', 'a[b-d]', 'ib');
select regexp_replace('aC', 'a[b-d]', '[\&]', 'ib');
select regexp_matches('aC', 'a[^b-d]', 'i');
select regexp_replace('aC', 'a[^b-d]', '[\&]', 'i');
select regexp_matches('aC', 'a[^b-d]', 'ib');
select regexp_replace('aC', 'a[^b-d]', '[\&]', 'ib');
select regexp_matches('aC', 'a[B-Z]', 'i');
select regexp_replace('aC', 'a[B-Z]', '[\&]', 'i');
select regexp_matches('aC', 'a[B-Z]', 'ib');
select regexp_replace('aC', 'a[B-Z]', '[\&]', 'ib');
select regexp_matches('aC', 'a[^B-Z]', 'i');
select regexp_replace('aC', 'a[^B-Z]', '[\&]', 'i');
select regexp_matches('aC', 'a[^B-Z]', 'ib');
select regexp_replace('aC', 'a[^B-Z]', '[\&]', 'ib');
select regexp_matches('', '***?', '');
select regexp_replace('', '***?', '[\&]', '');
select regexp_matches('', '***?', 'b');
select regexp_replace('', '***?', '[\&]', 'b');
select regexp_matches('***?', '***?', 'q');
select regexp_replace('***?', '***?', '[\&]', 'q');
select regexp_matches('a*b', '***=a*b', '');
select regexp_replace('a*b', '***=a*b', '[\&]', '');
select regexp_matches('a*b', '***=a*b', 'b');
select regexp_replace('a*b', '***=a*b', '[\&]', 'b');
select regexp_matches('***=a*b', '***=a*b', 'q');
select regexp_replace('***=a*b', '***=a*b', '[\&]', 'q');
select regexp_matches('ab', '***:\w+', 'b');
select regexp_replace('ab', '***:\w+', '[\&]', 'b');
select regexp_matches('ab', '(?e)***:\w+', '');
select regexp_replace('ab', '(?e)***:\w+', '[\&]', '');
select regexp_matches('', '***:***=a*b', '');
select regexp_replace('', '***:***=a*b', '[\&]', '');
select regexp_matches('', '***:***=a*b', 'b');
select regexp_replace('', '***:***=a*b', '[\&]', 'b');
select regexp_matches('a+b', '***:(?b)a+b', '');
select regexp_replace('a+b', '***:(?b)a+b', '[\&]', '');
select regexp_matches('a+b', '***:(?b)a+b', 'b');
select regexp_replace('a+b', '***:(?b)a+b', '[\&]', 'b');
select regexp_matches('a+b', '(?b)a+b', '');
select regexp_replace('a+b', '(?b)a+b', '[\&]', '');
select regexp_matches('', '(?e)(?b)\w+', '');
select regexp_replace('', '(?e)(?b)\w+', '[\&]', '');
select regexp_matches('(?b)w+', '(?b)\w+', 'b');
select regexp_replace('(?b)w+', '(?b)\w+', '[\&]', 'b');
select regexp_matches('a', '(?c)a', 'i');
select regexp_replace('a', '(?c)a', '[\&]', 'i');
select regexp_matches('A', '(?c)a', 'i');
select regexp_replace('A', '(?c)a', '[\&]', 'i');
select regexp_matches('WW', '(?e)\W+', '');
select regexp_replace('WW', '(?e)\W+', '[\&]', '');
select regexp_matches('Aa', '(?i)a+', '');
select regexp_replace('Aa', '(?i)a+', '[\&]', '');
select regexp_matches(E'a\nb', '(?m)a.b', '');
select regexp_replace(E'a\nb', '(?m)a.b', '[\&]', '');
select regexp_matches(E'a\nb', '(?m)^b', '');
select regexp_replace(E'a\nb', '(?m)^b', '[\&]', '');
select regexp_matches(E'a\nb', '(?n)a.b', '');
select regexp_replace(E'a\nb', '(?n)a.b', '[\&]', '');
select regexp_matches(E'a\nb', '(?n)^b', '');
select regexp_replace(E'a\nb', '(?n)^b', '[\&]', '');
select regexp_matches(E'a\nb', '(?p)a.b', '');
select regexp_replace(E'a\nb', '(?p)a.b', '[\&]', '');
select regexp_matches(E'a\nb', '(?p)^b', '');
select regexp_replace(E'a\nb', '(?p)^b', '[\&]', '');
select regexp_matches('a+b', '(?q)a+b', '');
select regexp_replace('a+b', '(?q)a+b', '[\&]', '');
select regexp_matches(E'a\nb', '(?s)a.b', 'n');
select regexp_replace(E'a\nb', '(?s)a.b', '[\&]', 'n');
select regexp_matches('a b', '(?t)a b', 'x');
select regexp_replace('a b', '(?t)a b', '[\&]', 'x');
select regexp_matches(E'a\nb', '(?w)a.b', '');
select regexp_replace(E'a\nb', '(?w)a.b', '[\&]', '');
select regexp_matches(E'a\nb', '(?w)^b', '');
select regexp_replace(E'a\nb', '(?w)^b', '[\&]', '');
select regexp_matches('ab', '(?x)a b', '');
select regexp_replace('ab', '(?x)a b', '[\&]', '');
select regexp_matches('', '(?z)ab', '');
select regexp_replace('', '(?z)ab', '[\&]', '');
select regexp_matches('Aa', '(?ici)a+', '');
select regexp_replace('Aa', '(?ici)a+', '[\&]', '');
select regexp_matches('', '(?i)(?q)a+', '');
select regexp_replace('', '(?i)(?q)a+', '[\&]', '');
select regexp_matches('(?i)a+', '(?q)(?i)a+', '');
select regexp_replace('(?i)a+', '(?q)(?i)a+', '[\&]', '');
select regexp_matches('a', '(?qe)a+', '');
select regexp_replace('a', '(?qe)a+', '[\&]', '');
select regexp_matches('a b', '(?q)a b', 'x');
select regexp_replace('a b', '(?q)a b', '[\&]', 'x');
select regexp_matches('a b', '(?qx)a b', '');
select regexp_replace('a b', '(?qx)a b', '[\&]', '');
select regexp_matches('Ab', '(?qi)ab', '');
select regexp_replace('Ab', '(?qi)ab', '[\&]', '');
select regexp_matches('abc', 'a(b)c', '');
select regexp_replace('abc', 'a(b)c', '[\&]', '');
select regexp_matches('xabc', 'a(?:b)c', '');
select regexp_replace('xabc', 'a(?:b)c', '[\&]', '');
select regexp_matches('xabcy', 'a((b))c', '');
select regexp_replace('xabcy', 'a((b))c', '[\&]', '');
select regexp_matches('abcy', 'a(?:(b))c', '');
select regexp_replace('abcy', 'a(?:(b))c', '[\&]', '');
select regexp_matches('abc', 'a((?:b))c', '');
select regexp_replace('abc', 'a((?:b))c', '[\&]', '');
select regexp_matches('abc', 'a(?:(?:b))c', '');
select regexp_replace('abc', 'a(?:(?:b))c', '[\&]', '');
select regexp_matches('ac', 'a(b){0}c', '');
select regexp_replace('ac', 'a(b){0}c', '[\&]', '');
select regexp_matches('abcde', 'a(b)c(d)e', '');
select regexp_replace('abcde', 'a(b)c(d)e', '[\&]', '');
select regexp_matches('bcde', '(b)c(d)e', '');
select regexp_replace('bcde', '(b)c(d)e', '[\&]', '');
select regexp_matches('abde', 'a(b)(d)e', '');
select regexp_replace('abde', 'a(b)(d)e', '[\&]', '');
select regexp_matches('abcd', 'a(b)c(d)', '');
select regexp_replace('abcd', 'a(b)c(d)', '[\&]', '');
select regexp_matches('xabcdy', '(ab)(cd)', '');
select regexp_replace('xabcdy', '(ab)(cd)', '[\&]', '');
select regexp_matches('xabcy', 'a(b)?c', '');
select regexp_replace('xabcy', 'a(b)?c', '[\&]', '');
select regexp_matches('xacy', 'a(b)?c', '');
select regexp_replace('xacy', 'a(b)?c', '[\&]', '');
select regexp_matches('xabcdey', 'a(b)?c(d)?e', '');
select regexp_replace('xabcdey', 'a(b)?c(d)?e', '[\&]', '');
select regexp_matches('xacdey', 'a(b)?c(d)?e', '');
select regexp_replace('xacdey', 'a(b)?c(d)?e', '[\&]', '');
select regexp_matches('xabcey', 'a(b)?c(d)?e', '');
select regexp_replace('xabcey', 'a(b)?c(d)?e', '[\&]', '');
select regexp_matches('xacey', 'a(b)?c(d)?e', '');
select regexp_replace('xacey', 'a(b)?c(d)?e', '[\&]', '');
select regexp_matches('xabcy', 'a(b)*c', '');
select regexp_replace('xabcy', 'a(b)*c', '[\&]', '');
select regexp_matches('xabbbcy', 'a(b)*c', '');
select regexp_replace('xabbbcy', 'a(b)*c', '[\&]', '');
select regexp_matches('xacy', 'a(b)*c', '');
select regexp_replace('xacy', 'a(b)*c', '[\&]', '');
select regexp_matches('xabbbcy', 'a(b*)c', '');
select regexp_replace('xabbbcy', 'a(b*)c', '[\&]', '');
select regexp_matches('xacy', 'a(b*)c', '');
select regexp_replace('xacy', 'a(b*)c', '[\&]', '');
select regexp_matches('xacy', 'a(b)+c', '');
select regexp_replace('xacy', 'a(b)+c', '[\&]', '');
select regexp_matches('xabcy', 'a(b)+c', '');
select regexp_replace('xabcy', 'a(b)+c', '[\&]', '');
select regexp_matches('xabbbcy', 'a(b)+c', '');
select regexp_replace('xabbbcy', 'a(b)+c', '[\&]', '');
select regexp_matches('xabbbcy', 'a(b+)c', '');
select regexp_replace('xabbbcy', 'a(b+)c', '[\&]', '');
select regexp_matches('xabbbcy', 'a(b){2,3}c', '');
select regexp_replace('xabbbcy', 'a(b){2,3}c', '[\&]', '');
select regexp_matches('xabbcy', 'a(b){2,3}c', '');
select regexp_replace('xabbcy', 'a(b){2,3}c', '[\&]', '');
select regexp_matches('xabcy', 'a(b){2,3}c', '');
select regexp_replace('xabcy', 'a(b){2,3}c', '[\&]', '');
select regexp_matches('-- abc-', '\y(\w+)\y', '');
select regexp_replace('-- abc-', '\y(\w+)\y', '[\&]', '');
select regexp_matches('abacdbd', 'a((b|c)d+)+', '');
select regexp_replace('abacdbd', 'a((b|c)d+)+', '[\&]', '');
select regexp_matches('abc', '(.*).*', '');
select regexp_replace('abc', '(.*).*', '[\&]', '');
select regexp_matches('bc', '(a*)*', '');
select regexp_replace('bc', '(a*)*', '[\&]', '');
select regexp_matches('asd TO foo', ' TO (([a-z0-9._]+|"([^"]+|"")+")+)', '');
select regexp_replace('asd TO foo', ' TO (([a-z0-9._]+|"([^"]+|"")+")+)', '[\&]', '');
select regexp_matches('xy', '((.))(\2){0}', '');
select regexp_replace('xy', '((.))(\2){0}', '[\&]', '');
select regexp_matches('xyy', '((.))(\2)', '');
select regexp_replace('xyy', '((.))(\2)', '[\&]', '');
select regexp_matches('xyy', '((.))(\2)', '');
select regexp_replace('xyy', '((.))(\2)', '[\&]', '');
select regexp_matches('xxx', '(.){0}(\1)', '');
select regexp_replace('xxx', '(.){0}(\1)', '[\&]', '');
select regexp_matches('xxx', '((.)){0}(\2)', '');
select regexp_replace('xxx', '((.)){0}(\2)', '[\&]', '');
select regexp_matches('xyz', '((.)){0}(\2){0}', '');
select regexp_replace('xyz', '((.)){0}(\2){0}', '[\&]', '');
select regexp_matches('ab', 'a(?=b)b*', '');
select regexp_replace('ab', 'a(?=b)b*', '[\&]', '');
select regexp_matches('a', 'a(?=b)b*', '');
select regexp_replace('a', 'a(?=b)b*', '[\&]', '');
select regexp_matches('abc', 'a(?=b)b*(?=c)c*', '');
select regexp_replace('abc', 'a(?=b)b*(?=c)c*', '[\&]', '');
select regexp_matches('ab', 'a(?=b)b*(?=c)c*', '');
select regexp_replace('ab', 'a(?=b)b*(?=c)c*', '[\&]', '');
select regexp_matches('ab', 'a(?!b)b*', '');
select regexp_replace('ab', 'a(?!b)b*', '[\&]', '');
select regexp_matches('a', 'a(?!b)b*', '');
select regexp_replace('a', 'a(?!b)b*', '[\&]', '');
select regexp_matches('b', '(?=b)b', '');
select regexp_replace('b', '(?=b)b', '[\&]', '');
select regexp_matches('a', '(?=b)b', '');
select regexp_replace('a', '(?=b)b', '[\&]', '');
select regexp_matches('abcde', '...(?!.)', '');
select regexp_replace('abcde', '...(?!.)', '[\&]', '');
select regexp_matches('abc', '...(?=.)', '');
select regexp_replace('abc', '...(?=.)', '[\&]', '');
select regexp_matches('ab', '(?<=a)b*', '');
select regexp_replace('ab', '(?<=a)b*', '[\&]', '');
select regexp_matches('b', '(?<=a)b*', '');
select regexp_replace('b', '(?<=a)b*', '[\&]', '');
select regexp_matches('abc', '(?<=a)b*(?<=b)c*', '');
select regexp_replace('abc', '(?<=a)b*(?<=b)c*', '[\&]', '');
select regexp_matches('ac', '(?<=a)b*(?<=b)c*', '');
select regexp_replace('ac', '(?<=a)b*(?<=b)c*', '[\&]', '');
select regexp_matches('ab', 'a(?<!a)b*', '');
select regexp_replace('ab', 'a(?<!a)b*', '[\&]', '');
select regexp_matches('a', 'a(?<!b)b*', '');
select regexp_replace('a', 'a(?<!b)b*', '[\&]', '');
select regexp_matches('bb', '(?<=b)b', '');
select regexp_replace('bb', '(?<=b)b', '[\&]', '');
select regexp_matches('b', '(?<=b)b', '');
select regexp_replace('b', '(?<=b)b', '[\&]', '');
select regexp_matches('abcde', '(?<=.)..', '');
select regexp_replace('abcde', '(?<=.)..', '[\&]', '');
select regexp_matches('aaabb', '(?<=..)a*', '');
select regexp_replace('aaabb', '(?<=..)a*', '[\&]', '');
select regexp_matches('aaabb', '(?<=..)b*', '');
select regexp_replace('aaabb', '(?<=..)b*', '[\&]', '');
select regexp_matches('aaabb', '(?<=..)b+', '');
select regexp_replace('aaabb', '(?<=..)b+', '[\&]', '');
select regexp_matches('abb', 'ab+?', '');
select regexp_replace('abb', 'ab+?', '[\&]', '');
select regexp_matches('abbc', 'ab+?c', '');
select regexp_replace('abbc', 'ab+?c', '[\&]', '');
select regexp_matches('abb', 'ab*?', '');
select regexp_replace('abb', 'ab*?', '[\&]', '');
select regexp_matches('abbc', 'ab*?c', '');
select regexp_replace('abbc', 'ab*?c', '[\&]', '');
select regexp_matches('ab', 'ab??', '');
select regexp_replace('ab', 'ab??', '[\&]', '');
select regexp_matches('abc', 'ab??c', '');
select regexp_replace('abc', 'ab??c', '[\&]', '');
select regexp_matches('abbbb', 'ab{2,4}?', '');
select regexp_replace('abbbb', 'ab{2,4}?', '[\&]', '');
select regexp_matches('abbbbc', 'ab{2,4}?c', '');
select regexp_replace('abbbbc', 'ab{2,4}?c', '[\&]', '');
select regexp_matches('123zzzz456', '3z*', '');
select regexp_replace('123zzzz456', '3z*', '[\&]', '');
select regexp_matches('123zzzz456', '3z*?', '');
select regexp_replace('123zzzz456', '3z*?', '[\&]', '');
select regexp_matches('123zzzz456', 'z*4', '');
select regexp_replace('123zzzz456', 'z*4', '[\&]', '');
select regexp_matches('123zzzz456', 'z*?4', '');
select regexp_replace('123zzzz456', 'z*?4', '[\&]', '');
select regexp_matches('foo/bar/baz', '^([^/]+?)(?:/([^/]+?))(?:/([^/]+?))?$', '');
select regexp_replace('foo/bar/baz', '^([^/]+?)(?:/([^/]+?))(?:/([^/]+?))?$', '[\&]', '');
select regexp_matches('foo/bar/baz/quux', '^(.+?)(?:/(.+?))(?:/(.+?)\3)?$', '');
select regexp_replace('foo/bar/baz/quux', '^(.+?)(?:/(.+?))(?:/(.+?)\3)?$', '[\&]', '');
select regexp_matches('xyza', '^(.*?)(a*)$', '');
select regexp_replace('xyza', '^(.*?)(a*)$', '[\&]', '');
select regexp_matches('xyzaa', '^(.*?)(a*)$', '');
select regexp_replace('xyzaa', '^(.*?)(a*)$', '[\&]', '');
select regexp_matches('xyz', '^(.*?)(a*)$', '');
select regexp_replace('xyz', '^(.*?)(a*)$', '[\&]', '');
select regexp_matches('weeknights', '(week|wee)(night|knights)', '');
select regexp_replace('weeknights', '(week|wee)(night|knights)', '[\&]', '');
select regexp_matches('abccbccb', 'a(bc*).*\1', '');
select regexp_replace('abccbccb', 'a(bc*).*\1', '[\&]', '');
select regexp_matches('abcbd', 'a(b.[bc]*)+', '');
select regexp_replace('abcbd', 'a(b.[bc]*)+', '[\&]', '');
select regexp_matches('abc', 'a(?:b|b)c', '');
select regexp_replace('abc', 'a(?:b|b)c', '[\&]', '');
select regexp_matches('aba', '[ab][ab][ab]', '');
select regexp_replace('aba', '[ab][ab][ab]', '[\&]', '');
select regexp_matches('aba', '[ab][ab][ab]', 'b');
select regexp_replace('aba', '[ab][ab][ab]', '[\&]', 'b');
select regexp_matches('abababa', '[ab][ab][ab][ab][ab][ab][ab]', '');
select regexp_replace('abababa', '[ab][ab][ab][ab][ab][ab][ab]', '[\&]', '');
select regexp_matches('abababa', '[ab][ab][ab][ab][ab][ab][ab]', 'b');
select regexp_replace('abababa', '[ab][ab][ab][ab][ab][ab][ab]', '[\&]', 'b');
select regexp_matches('abcdefghijkl', 'abcdefghijkl', '');
select regexp_replace('abcdefghijkl', 'abcdefghijkl', '[\&]', '');
select regexp_matches('abcdefghijkl', 'abcdefghijkl', 'b');
select regexp_replace('abcdefghijkl', 'abcdefghijkl', '[\&]', 'b');
select regexp_matches('agn', 'a(?:b|c|d|e|f|g|h|i|j|k|l|m)n', '');
select regexp_replace('agn', 'a(?:b|c|d|e|f|g|h|i|j|k|l|m)n', '[\&]', '');
select regexp_matches('abc', 'a(((((((((((((b)))))))))))))c', '');
select regexp_replace('abc', 'a(((((((((((((b)))))))))))))c', '[\&]', '');
select regexp_matches('abbc', 'ab{1,100}c', '');
select regexp_replace('abbc', 'ab{1,100}c', '[\&]', '');
select regexp_matches('abbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbc', 'ab{1,100}c', '');
select regexp_replace('abbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbc', 'ab{1,100}c', '[\&]', '');
select regexp_matches('abbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbc', 'ab{1,100}c', '');
select regexp_replace('abbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbc', 'ab{1,100}c', '[\&]', '');
select regexp_matches('xyzabcdefgh', '\w+abcdefgh', '');
select regexp_replace('xyzabcdefgh', '\w+abcdefgh', '[\&]', '');
select regexp_matches('', '***', '');
select regexp_replace('', '***', '[\&]', '');
select regexp_matches('', '***', 'b');
select regexp_replace('', '***', '[\&]', 'b');
select regexp_matches('abb', 'a?b*', '');
select regexp_replace('abb', 'a?b*', '[\&]', '');
select regexp_matches('bb', 'a?b*', '');
select regexp_replace('bb', 'a?b*', '[\&]', '');
select regexp_matches('aab', 'a*b', '');
select regexp_replace('aab', 'a*b', '[\&]', '');
select regexp_matches('aab', 'a*b', 'b');
select regexp_replace('aab', 'a*b', '[\&]', 'b');
select regexp_matches('aaaab', '^a*b', '');
select regexp_replace('aaaab', '^a*b', '[\&]', '');
select regexp_matches('aaaab', '^a*b', 'b');
select regexp_replace('aaaab', '^a*b', '[\&]', 'b');
select regexp_matches('010010', '[0-6][1-2][0-3][0-6][1-6][0-6]', '');
select regexp_replace('010010', '[0-6][1-2][0-3][0-6][1-6][0-6]', '[\&]', '');
select regexp_matches('010010', '[0-6][1-2][0-3][0-6][1-6][0-6]', 'b');
select regexp_replace('010010', '[0-6][1-2][0-3][0-6][1-6][0-6]', '[\&]', 'b');
select regexp_matches(E'tk\n\n#\n#\nit0', '(?n)^(?![t#])\S+', '');
select regexp_replace(E'tk\n\n#\n#\nit0', '(?n)^(?![t#])\S+', '[\&]', '');
select regexp_matches('2:::DebugWin32', '([[:xdigit:]])([[:space:]]*)', '');
select regexp_replace('2:::DebugWin32', '([[:xdigit:]])([[:space:]]*)', '[\&]', '');
select regexp_matches('*%V2', '[ ]*(^|[^%])%V', '');
select regexp_replace('*%V2', '[ ]*(^|[^%])%V', '[\&]', '');
select regexp_matches('bbcos_001_c01.q1la', '([^_.]*)([^.]*)\.(..)(.).*', '');
select regexp_replace('bbcos_001_c01.q1la', '([^_.]*)([^.]*)\.(..)(.).*', '[\&]', '');
select regexp_matches('a<a>', '\A\s*[^<]*\s*<([^>]+)>', '');
select regexp_replace('a<a>', '\A\s*[^<]*\s*<([^>]+)>', '[\&]', '');
select regexp_matches('ab', '\A\s*([^b]*)b', '');
select regexp_replace('ab', '\A\s*([^b]*)b', '[\&]', '');
select regexp_matches('ab', '\A\s*[^b]*(b)', '');
select regexp_replace('ab', '\A\s*[^b]*(b)', '[\&]', '');
select regexp_matches('ab', '\A(\s*)[^b]*(b)', '');
select regexp_replace('ab', '\A(\s*)[^b]*(b)', '[\&]', '');
select regexp_matches('ab', '\A\s*[^b]*b', '');
select regexp_replace('ab', '\A\s*[^b]*b', '[\&]', '');
select regexp_matches('ab', '\A\s*[^b]*b', '');
select regexp_replace('ab', '\A\s*[^b]*b', '[\&]', '');
select regexp_matches('ab', '\A\s*[^b]*b', '');
select regexp_replace('ab', '\A\s*[^b]*b', '[\&]', '');
select regexp_matches(E'\n.b', '(^|\n)+\.*b', '');
select regexp_replace(E'\n.b', '(^|\n)+\.*b', '[\&]', '');
select regexp_matches(E'TQ\r\n.?<5000267>Test already stopped\r\n', '(^|[\n\r]+)\.*\?<.*?(\n|\r)+', '');
select regexp_replace(E'TQ\r\n.?<5000267>Test already stopped\r\n', '(^|[\n\r]+)\.*\?<.*?(\n|\r)+', '[\&]', '');
select regexp_matches('\3161573148', '\3161573148', '');
select regexp_replace('\3161573148', '\3161573148', '[\&]', '');
select regexp_matches('x', '($|^)*', '');
select regexp_replace('x', '($|^)*', '[\&]', '');
select regexp_matches('x', '(x{200}){200}$y', '');
select regexp_replace('x', '(x{200}){200}$y', '[\&]', '');
select regexp_matches('', $$
	^TETRA_MODE_CMD				# Message Type
	([[:blank:]]+)				# Pad
	(ETS_1_1|ETS_1_2|ETS_2_2)		# SystemCode
	([[:blank:]]+)				# Pad
	(CONTINUOUS|CARRIER|MCCH|TRAFFIC)	# SharingMode
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,2})			# ColourCode
	([[:blank:]]+)				# Pad
	(1|2|3|4|6|9|12|18)			# TSReservedFrames
	([[:blank:]]+)				# Pad
	(PASS|TRUE|FAIL|FALSE)			# UPlaneDTX
	([[:blank:]]+)				# Pad
	(PASS|TRUE|FAIL|FALSE)			# Frame18Extension
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,4})			# MCC
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,5})			# MNC
	([[:blank:]]+)				# Pad
	(BOTH|BCAST|ENQRY|NONE)			# NbrCellBcast
	([[:blank:]]+)				# Pad
	(UNKNOWN|LOW|MEDIUM|HIGH)		# CellServiceLevel
	([[:blank:]]+)				# Pad
	(PASS|TRUE|FAIL|FALSE)			# LateEntryInfo
	([[:blank:]]+)				# Pad
	(300|400)				# FrequencyBand
	([[:blank:]]+)				# Pad
	(NORMAL|REVERSE)			# ReverseOperation
	([[:blank:]]+)				# Pad
	(NONE|\+6\.25|\-6\.25|\+12\.5)		# Offset
	([[:blank:]]+)				# Pad
	(10)					# DuplexSpacing
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,4})			# MainCarrierNr
	([[:blank:]]+)				# Pad
	(0|1|2|3)				# NrCSCCH
	([[:blank:]]+)				# Pad
	(15|20|25|30|35|40|45)			# MSTxPwrMax
	([[:blank:]]+)				# Pad
	(\-125|\-120|\-115|\-110|\-105|\-100|\-95|\-90|\-85|\-80|\-75|\-70|\-65|\-60|\-55|\-50)
						# RxLevAccessMin
	([[:blank:]]+)				# Pad
	(\-53|\-51|\-49|\-47|\-45|\-43|\-41|\-39|\-37|\-35|\-33|\-31|\-29|\-27|\-25|\-23)
						# AccessParameter
	([[:blank:]]+)				# Pad
	(DISABLE|[[:digit:]]{3,4})		# RadioDLTimeout
	([[:blank:]]+)				# Pad
	(\-[[:digit:]]{2,3})			# RSSIThreshold
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,5})			# CCKIdSCKVerNr
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,5})			# LocationArea
	([[:blank:]]+)				# Pad
	([(1|0)]{16})				# SubscriberClass
	([[:blank:]]+)				# Pad
	([(1|0)]{12})				# BSServiceDetails
	([[:blank:]]+)				# Pad
	(RANDOMIZE|IMMEDIATE|[[:digit:]]{1,2})	# IMM
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,2})			# WT
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,2})			# Nu
	([[:blank:]]+)				# Pad
	([0-1])					# FrameLngFctr
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,2})			# TSPtr
	([[:blank:]]+)				# Pad
	([0-7])					# MinPriority
	([[:blank:]]+)				# Pad
	(PASS|TRUE|FAIL|FALSE)			# ExtdSrvcsEnabled
	([[:blank:]]+)				# Pad
	(.*)					# ConditionalFields
    $$, 'x');
select regexp_replace('', $$
	^TETRA_MODE_CMD				# Message Type
	([[:blank:]]+)				# Pad
	(ETS_1_1|ETS_1_2|ETS_2_2)		# SystemCode
	([[:blank:]]+)				# Pad
	(CONTINUOUS|CARRIER|MCCH|TRAFFIC)	# SharingMode
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,2})			# ColourCode
	([[:blank:]]+)				# Pad
	(1|2|3|4|6|9|12|18)			# TSReservedFrames
	([[:blank:]]+)				# Pad
	(PASS|TRUE|FAIL|FALSE)			# UPlaneDTX
	([[:blank:]]+)				# Pad
	(PASS|TRUE|FAIL|FALSE)			# Frame18Extension
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,4})			# MCC
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,5})			# MNC
	([[:blank:]]+)				# Pad
	(BOTH|BCAST|ENQRY|NONE)			# NbrCellBcast
	([[:blank:]]+)				# Pad
	(UNKNOWN|LOW|MEDIUM|HIGH)		# CellServiceLevel
	([[:blank:]]+)				# Pad
	(PASS|TRUE|FAIL|FALSE)			# LateEntryInfo
	([[:blank:]]+)				# Pad
	(300|400)				# FrequencyBand
	([[:blank:]]+)				# Pad
	(NORMAL|REVERSE)			# ReverseOperation
	([[:blank:]]+)				# Pad
	(NONE|\+6\.25|\-6\.25|\+12\.5)		# Offset
	([[:blank:]]+)				# Pad
	(10)					# DuplexSpacing
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,4})			# MainCarrierNr
	([[:blank:]]+)				# Pad
	(0|1|2|3)				# NrCSCCH
	([[:blank:]]+)				# Pad
	(15|20|25|30|35|40|45)			# MSTxPwrMax
	([[:blank:]]+)				# Pad
	(\-125|\-120|\-115|\-110|\-105|\-100|\-95|\-90|\-85|\-80|\-75|\-70|\-65|\-60|\-55|\-50)
						# RxLevAccessMin
	([[:blank:]]+)				# Pad
	(\-53|\-51|\-49|\-47|\-45|\-43|\-41|\-39|\-37|\-35|\-33|\-31|\-29|\-27|\-25|\-23)
						# AccessParameter
	([[:blank:]]+)				# Pad
	(DISABLE|[[:digit:]]{3,4})		# RadioDLTimeout
	([[:blank:]]+)				# Pad
	(\-[[:digit:]]{2,3})			# RSSIThreshold
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,5})			# CCKIdSCKVerNr
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,5})			# LocationArea
	([[:blank:]]+)				# Pad
	([(1|0)]{16})				# SubscriberClass
	([[:blank:]]+)				# Pad
	([(1|0)]{12})				# BSServiceDetails
	([[:blank:]]+)				# Pad
	(RANDOMIZE|IMMEDIATE|[[:digit:]]{1,2})	# IMM
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,2})			# WT
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,2})			# Nu
	([[:blank:]]+)				# Pad
	([0-1])					# FrameLngFctr
	([[:blank:]]+)				# Pad
	([[:digit:]]{1,2})			# TSPtr
	([[:blank:]]+)				# Pad
	([0-7])					# MinPriority
	([[:blank:]]+)				# Pad
	(PASS|TRUE|FAIL|FALSE)			# ExtdSrvcsEnabled
	([[:blank:]]+)				# Pad
	(.*)					# ConditionalFields
    $$, '[\&]', 'x');
select regexp_matches('', E'^MRK:client1: =1339 14HKelly Talisman 10011000 ([0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]*) [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 8 0 8 0 0 0 77 77 1 1 2 0 11 { 1 3 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 13HC6 My Creator 2 3 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 31HC7 Slightly offensive name, huh 3 8 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 23HE-mail:kelly@hotbox.com 4 9 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 17Hcompface must die 5 10 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 0 3HAir 6 12 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 14HPGP public key 7 13 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 16Hkelly@hotbox.com 8 30 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 0 12H2 text/plain 9 30 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 0 13H2 x-kom/basic 10 33 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 1H0 11 14 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 1H3 }\r?', '');
select regexp_replace('', E'^MRK:client1: =1339 14HKelly Talisman 10011000 ([0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]*) [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 8 0 8 0 0 0 77 77 1 1 2 0 11 { 1 3 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 13HC6 My Creator 2 3 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 31HC7 Slightly offensive name, huh 3 8 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 23HE-mail:kelly@hotbox.com 4 9 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 17Hcompface must die 5 10 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 0 3HAir 6 12 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 14HPGP public key 7 13 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 16Hkelly@hotbox.com 8 30 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 0 12H2 text/plain 9 30 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 0 13H2 x-kom/basic 10 33 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 1H0 11 14 8 [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* [0-9]* 00000000 1 1H3 }\r?', '[\&]', '');
select regexp_matches('x', '(^)+^', '');
select regexp_replace('x', '(^)+^', '[\&]', '');
select regexp_matches('x', '($^)+', '');
select regexp_replace('x', '($^)+', '[\&]', '');
select regexp_matches('x', '(^$)*', '');
select regexp_replace('x', '(^$)*', '[\&]', '');
select regexp_matches('aa bb cc', '(^(?!aa))+', '');
select regexp_replace('aa bb cc', '(^(?!aa))+', '[\&]', '');
select regexp_matches('aa x', '(^(?!aa)(?!bb)(?!cc))+', '');
select regexp_replace('aa x', '(^(?!aa)(?!bb)(?!cc))+', '[\&]', '');
select regexp_matches('bb x', '(^(?!aa)(?!bb)(?!cc))+', '');
select regexp_replace('bb x', '(^(?!aa)(?!bb)(?!cc))+', '[\&]', '');
select regexp_matches('cc x', '(^(?!aa)(?!bb)(?!cc))+', '');
select regexp_replace('cc x', '(^(?!aa)(?!bb)(?!cc))+', '[\&]', '');
select regexp_matches('dd x', '(^(?!aa)(?!bb)(?!cc))+', '');
select regexp_replace('dd x', '(^(?!aa)(?!bb)(?!cc))+', '[\&]', '');
select regexp_matches('x', 'abcd(\m)+xyz', '');
select regexp_replace('x', 'abcd(\m)+xyz', '[\&]', '');
select regexp_matches('a', 'abcd(\m)+xyz', '');
select regexp_replace('a', 'abcd(\m)+xyz', '[\&]', '');
select regexp_matches('x', '^abcd*(((((^(a c(e?d)a+|)+|)+|)+|)+|a)+|)', '');
select regexp_replace('x', '^abcd*(((((^(a c(e?d)a+|)+|)+|)+|)+|a)+|)', '[\&]', '');
select regexp_matches('x', 'a^(^)bcd*xy(((((($a+|)+|)+|)+$|)+|)+|)^$', '');
select regexp_replace('x', 'a^(^)bcd*xy(((((($a+|)+|)+|)+$|)+|)+|)^$', '[\&]', '');
select regexp_matches('x', 'xyz(\Y\Y)+', '');
select regexp_replace('x', 'xyz(\Y\Y)+', '[\&]', '');
select regexp_matches('x', 'x|(?:\M)+', '');
select regexp_replace('x', 'x|(?:\M)+', '[\&]', '');
select regexp_matches('x', repeat('x*y*z*', 200), '');
select regexp_replace('x', repeat('x*y*z*', 200), '[\&]', '');
select regexp_matches('foo', '(\Y)+', '');
select regexp_replace('foo', '(\Y)+', '[\&]', '');
select regexp_matches('a45', 'a\Y(?=45)', '');
select regexp_replace('a45', 'a\Y(?=45)', '[\&]', '');
select regexp_matches('ac', 'a(?=.)c', '');
select regexp_replace('ac', 'a(?=.)c', '[\&]', '');
select regexp_matches('azz33', 'a(?=.).*(?=3)3*', '');
select regexp_replace('azz33', 'a(?=.).*(?=3)3*', '[\&]', '');
select regexp_matches('az3%', 'a(?=\w)\w*(?=.).*', '');
select regexp_replace('az3%', 'a(?=\w)\w*(?=.).*', '[\&]', '');
select regexp_matches('ABCDEFGHIJKLMNOPQRSTUVWXYZ3', 'ABCDEFGHIJKLMNOPQRSTUVWXYZ(?:\w|a|b|c|d|e|f|0|1|2|3|4|5|6|Q)', '');
select regexp_replace('ABCDEFGHIJKLMNOPQRSTUVWXYZ3', 'ABCDEFGHIJKLMNOPQRSTUVWXYZ(?:\w|a|b|c|d|e|f|0|1|2|3|4|5|6|Q)', '[\&]', '');
select regexp_matches('ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789Z', 'ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789(\Y\Y)+', '');
select regexp_replace('ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789Z', 'ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789(\Y\Y)+', '[\&]', '');
select regexp_matches('az', '((x|xabcdefghijklmnopqrstuvwxyz0123456789)x*|[^y]z)$', '');
select regexp_replace('az', '((x|xabcdefghijklmnopqrstuvwxyz0123456789)x*|[^y]z)$', '[\&]', '');
select regexp_matches(E'a\u0102\u02ffb', 'a[\u00fe-\u0507][\u00ff-\u0300]b', '');
select regexp_replace(E'a\u0102\u02ffb', 'a[\u00fe-\u0507][\u00ff-\u0300]b', '[\&]', '');
select regexp_matches(E'a\u1234x', 'a\U00001234x', '');
select regexp_replace(E'a\u1234x', 'a\U00001234x', '[\&]', '');
select regexp_matches(E'a\u1234x', 'a\U00001234x', '');
select regexp_replace(E'a\u1234x', 'a\U00001234x', '[\&]', '');
select regexp_matches(E'a\u1234x', 'a\U0001234x', '');
select regexp_replace(E'a\u1234x', 'a\U0001234x', '[\&]', '');
select regexp_matches(E'a\u1234x', 'a\U0001234x', '');
select regexp_replace(E'a\u1234x', 'a\U0001234x', '[\&]', '');
select regexp_matches(E'a\u12345x', 'a\U000012345x', '');
select regexp_replace(E'a\u12345x', 'a\U000012345x', '[\&]', '');
select regexp_matches(E'a\u12345x', 'a\U000012345x', '');
select regexp_replace(E'a\u12345x', 'a\U000012345x', '[\&]', '');
select regexp_matches(E'a\ufffd0x', 'a\U1000000x', '');
select regexp_replace(E'a\ufffd0x', 'a\U1000000x', '[\&]', '');
select regexp_matches(E'a\ufffd0x', 'a\U1000000x', '');
select regexp_replace(E'a\ufffd0x', 'a\U1000000x', '[\&]', '');
select regexp_matches(E'a\u1234\u1236\u1236x', 'a
  [\u1000-\u1100]*
  [\u3000-\u3100]*
  [\u1234-\u25ff]+
  [\u2000-\u35ff]*
  [\u2600-\u2f00]*
  \u1236\u1236x', 'x');
select regexp_replace(E'a\u1234\u1236\u1236x', 'a
  [\u1000-\u1100]*
  [\u3000-\u3100]*
  [\u1234-\u25ff]+
  [\u2000-\u35ff]*
  [\u2600-\u2f00]*
  \u1236\u1236x', '[\&]', 'x');
select regexp_matches(E'\u1500\u1237', '[[:alnum:]]*[[:upper:]]*[\u1000-\u2000]*\u1237', '');
select regexp_replace(E'\u1500\u1237', '[[:alnum:]]*[[:upper:]]*[\u1000-\u2000]*\u1237', '[\&]', '');
select regexp_matches(E'A\u1239', '[[:alnum:]]*[[:upper:]]*[\u1000-\u2000]*\u1237', '');
select regexp_replace(E'A\u1239', '[[:alnum:]]*[[:upper:]]*[\u1000-\u2000]*\u1237', '[\&]', '');
select regexp_matches(E'\u1500\u1237', '[[:alnum:]]*[[:upper:]]*[\u1000-\u2000]*\u1237', 'i');
select regexp_replace(E'\u1500\u1237', '[[:alnum:]]*[[:upper:]]*[\u1000-\u2000]*\u1237', '[\&]', 'i');
select regexp_matches(E'x*\u1500\u1237', '[[:alnum:]]+', '');
select regexp_replace(E'x*\u1500\u1237', '[[:alnum:]]+', '[\&]', '');
select regexp_matches(E'x*\u1500\u1237', '[[:alpha:]]+', '');
select regexp_replace(E'x*\u1500\u1237', '[[:alpha:]]+', '[\&]', '');
select regexp_matches(E'x\u1500\u1237', '[[:ascii:]]+', '');
select regexp_replace(E'x\u1500\u1237', '[[:ascii:]]+', '[\&]', '');
select regexp_matches(E'x \t\u1500\u1237', '[[:blank:]]+', '');
select regexp_replace(E'x \t\u1500\u1237', '[[:blank:]]+', '[\&]', '');
select regexp_matches(E'x\u1500\u1237', '[[:cntrl:]]+', '');
select regexp_replace(E'x\u1500\u1237', '[[:cntrl:]]+', '[\&]', '');
select regexp_matches(E'x9\u1500\u1237', '[[:digit:]]+', '');
select regexp_replace(E'x9\u1500\u1237', '[[:digit:]]+', '[\&]', '');
select regexp_matches(E'x\u1500\u1237' COLLATE pg_c_utf8, '[[:graph:]]+', '');
select regexp_replace(E'x\u1500\u1237' COLLATE pg_c_utf8, '[[:graph:]]+', '[\&]', '');
select regexp_matches(E'x\u1500\u1237', '[[:lower:]]+', '');
select regexp_replace(E'x\u1500\u1237', '[[:lower:]]+', '[\&]', '');
select regexp_matches(E'x\u1500\u1237' COLLATE pg_c_utf8, '[[:print:]]+', '');
select regexp_replace(E'x\u1500\u1237' COLLATE pg_c_utf8, '[[:print:]]+', '[\&]', '');
select regexp_matches(E'x.\u1500\u1237', '[[:punct:]]+', '');
select regexp_replace(E'x.\u1500\u1237', '[[:punct:]]+', '[\&]', '');
select regexp_matches(E'x \t\u1500\u1237', '[[:space:]]+', '');
select regexp_replace(E'x \t\u1500\u1237', '[[:space:]]+', '[\&]', '');
select regexp_matches(E'xX\u1500\u1237', '[[:upper:]]+', '');
select regexp_replace(E'xX\u1500\u1237', '[[:upper:]]+', '[\&]', '');
select regexp_matches(E'xa9\u1500\u1237', '[[:xdigit:]]+', '');
select regexp_replace(E'xa9\u1500\u1237', '[[:xdigit:]]+', '[\&]', '');
select regexp_matches(E'x_*\u1500\u1237', '[[:word:]]+', '');
select regexp_replace(E'x_*\u1500\u1237', '[[:word:]]+', '[\&]', '');
