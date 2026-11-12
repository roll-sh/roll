# Testcontainers for Python (Cassandra and ScyllaDB modules) with the roll image swapped in.
FROM python:3.12-slim@sha256:5e2bdb1b0d542d5039cffb8181c65930093949194a65c365dfb1b0fe13a4fa5b
RUN pip install --no-cache-dir "testcontainers==4.15.0" "cassandra-driver==3.30.1"
COPY test_tc.py /test_tc.py
ENTRYPOINT ["python", "-u", "/test_tc.py"]
