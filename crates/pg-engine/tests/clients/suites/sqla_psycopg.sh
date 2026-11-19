# SQLAlchemy's suite (sqlalchemy.sh) with the psycopg driver.
# image: python:3.12-slim
# src: https://github.com/sqlalchemy/sqlalchemy/archive/refs/tags/rel_2_0_41.tar.gz
# setup: sqlalchemy
SQLA_DRIVER=psycopg . /harness/suites/sqlalchemy.sh
