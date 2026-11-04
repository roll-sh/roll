CREATE TEMP TABLE a162_copy_fixture (c0 text, c1 text, c2 text, c3 text, c4 text, c5 text, c6 text, c7 text, c8 text, c9 text, c10 text, c11 text, c12 text, c13 text, c14 text, c15 text);
COPY a162_copy_fixture FROM '/home/meszmate/.cache/a162/pgr/resume-3997cf00/src-repo/src/test/regress/data/onek.data';
SELECT 'rows=' || count(*) FROM a162_copy_fixture;
