# Testcontainers for Python (Cassandra and ScyllaDB modules) with the roll image swapped in.
FROM python:3.12-slim
RUN pip install --no-cache-dir "testcontainers==4.15.0" "cassandra-driver==3.30.1"
COPY test_tc.py /test_tc.py
ENTRYPOINT ["python", "-u", "/test_tc.py"]
