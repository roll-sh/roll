# The Python driver suite. DRIVER picks the package: cassandra-driver (DataStax/Apache) or
# scylla-driver (ScyllaDB's fork); both install the `cassandra` module.
FROM python:3.12-slim
ARG DRIVER=cassandra-driver==3.30.1
RUN pip install --no-cache-dir "$DRIVER"
COPY suite.py /suite.py
ENTRYPOINT ["python", "-u", "/suite.py"]
