-- Errors, as cqlsh prints them (with the file name and line number).
DROP KEYSPACE IF EXISTS cli_g_errors;
CREATE KEYSPACE cli_g_errors WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1};
SELEC 1;
SELECT * FROM cli_g_errors.nope;
SELECT nope FROM system.local;
USE nope;
USE select;
USE cli_g_errors extra;
CREATE KEYSPACE cli_g_errors WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1};
CREATE TABLE cli_g_errors.t (k int PRIMARY KEY, v text);
CREATE TABLE cli_g_errors.t (k int PRIMARY KEY, v text);
INSERT INTO cli_g_errors.t (k, v) VALUES ('x', 1);
INSERT INTO cli_g_errors.t (v) VALUES ('no key');
SELECT * FROM cli_g_errors.t WHERE v = 'x';
SELECT & FROM cli_g_errors.t;
SELECT *
  FROM cli_g_errors.t
  WHERE k = # 1;
CONSISTENCY FOO;
CONSISTENCY;
SERIAL CONSISTENCY QUORUM;
EXPAND bogus;
TRACING maybe;
PAGING -1;
SHOW nonsense;
SHOW SESSION;
SOURCE;
SOURCE '/tmp/roll-cqlsh-golden/does-not-exist.cql';
CAPTURE OFF;
CAPTURE bogus;
COPY;
COPY cli_g_errors.t SIDEWAYS STDOUT;
COPY cli_g_errors.nope TO STDOUT;
COPY nope.t TO STDOUT;
COPY cli_g_errors.t (k, nope) TO STDOUT;
DESC;
DESC foo bar;
UNICODE;
foo bar;
DROP KEYSPACE cli_g_errors;
SELECT 'unterminated
