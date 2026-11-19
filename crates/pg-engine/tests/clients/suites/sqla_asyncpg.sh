# SQLAlchemy's suite (sqlalchemy.sh) with the asyncpg driver.
# image: python:3.12-slim
# src: https://github.com/sqlalchemy/sqlalchemy/archive/refs/tags/rel_2_0_41.tar.gz
# setup: sqlalchemy
SQLA_DRIVER=asyncpg . /harness/suites/sqlalchemy.sh
