-- cqlsh-only commands, statement splitting and comments.
DROP KEYSPACE IF EXISTS cli_g_shell;
CREATE KEYSPACE cli_g_shell WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1};
CREATE TABLE cli_g_shell.t (k int PRIMARY KEY, v text);
// a comment
-- another
/* block */ INSERT INTO cli_g_shell.t (k, v) VALUES (1, 'a;b'); INSERT INTO cli_g_shell.t (k, v) VALUES (2, $$it's $$);
SELECT * FROM cli_g_shell.t
  WHERE k = 1 -- trailing comment
  ;
BEGIN BATCH
  INSERT INTO cli_g_shell.t (k, v) VALUES (3, 'x');
  INSERT INTO cli_g_shell.t (k, v) VALUES (4, 'y');
APPLY BATCH;
SELECT * FROM cli_g_shell.t WHERE k IN (1, 2, 3, 4);
CONSISTENCY
CONSISTENCY quorum
CONSISTENCY;
CONSISTENCY LOCAL_ONE;
SERIAL CONSISTENCY;
SERIAL CONSISTENCY local_serial;
SERIAL CONSISTENCY;
TRACING
TRACING OFF
TRACING ON
TRACING ON
TRACING OFF
EXPAND
EXPAND ON
EXPAND ON
SELECT * FROM cli_g_shell.t WHERE k = 1;
EXPAND OFF
EXPAND OFF
PAGING
PAGING ON
PAGING 2
PAGING OFF
PAGING OFF
PAGING
PAGING 50
USE cli_g_shell;
SELECT v FROM t WHERE k = 2;
USE "cli_g_shell";
SHOW VERSION;
HELP;
HELP CONSISTENCY;
HELP EXPAND PAGING;
HELP nope;
? TRACING
CAPTURE
CAPTURE '/tmp/roll-cqlsh-golden/capture.txt'
SELECT * FROM t WHERE k = 1;
CAPTURE
CAPTURE '/tmp/roll-cqlsh-golden/capture2.txt'
SELECT * FROM t WHERE k = 3;
CAPTURE OFF
CAPTURE OFF
SELECT * FROM t WHERE k = 4;
SOURCE '/tmp/roll-cqlsh-golden/sourced.cql'
SELECT * FROM t WHERE k = 99;
DROP KEYSPACE cli_g_shell;
