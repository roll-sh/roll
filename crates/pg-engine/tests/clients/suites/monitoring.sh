# What monitoring tools ask a server, compared query by query (suites/monitoring.py): pgwatch
# 3.7.0's metrics, pg_activity 3.6.1's queries, pganalyze-collector 0.66.1's, Datadog's
# postgres integration 22.12.0's and check_postgres's, their SQL taken from the tools' own
# sources (fetched into $SRC by the script), run with psycopg 3.2.9. Ids
# `<tool>/<query>#<hash>` over the error's SQLSTATE or the columns, their types and NULLs.
# Database: monitoring.setup.sql (tables, indexes, a view, statistics).
# image: python:3.13-slim
# src:
set -eu
V=/cache/venv/monitoring
if [ ! -x $V/bin/python ]; then
  python -m venv $V
  $V/bin/pip install -q psycopg==3.2.9 psycopg-binary==3.2.9 pyyaml==6.0.2
fi
$V/bin/python /harness/suites/monitoring.py
