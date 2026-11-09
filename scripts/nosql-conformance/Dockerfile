# Runner image for ScyllaDB's cqlpy and alternator suites (scripts/nosql-conformance.sh).
# The suites themselves are mounted at run time and never baked in.
FROM docker:27-cli@sha256:851f91d241214e7c6db86513b270d58776379aacc5eb9c4a87e5b47115e3065c AS dockercli

FROM python:3.12-slim@sha256:5e2bdb1b0d542d5039cffb8181c65930093949194a65c365dfb1b0fe13a4fa5b
# The docker CLI lets Cassandra-targeted runs reach nodetool inside the reference (nodetool-docker).
COPY --from=dockercli /usr/local/bin/docker /usr/local/bin/docker
# scylla-driver matches the version bundled with the reference image's cqlsh.
RUN pip install --no-cache-dir \
      scylla-driver==3.29.7 \
      pytest==8.4.2 \
      pytest-asyncio==1.4.0 \
      pytest-timeout==2.4.0 \
      pytest-xdist==3.8.0 \
      allure-pytest==2.16.2 \
      boto3==1.43.115 \
      requests==2.34.2 \
      PyYAML==6.0.3 \
      aiohttp==3.14.5 \
      psutil==7.2.2 \
      tabulate==0.10.0 \
      unidiff==1.0.1 \
      universalasync==0.5.1.0 \
      colorama==0.4.6 \
      cryptography==50.0.2 \
      packaging==26.3 \
      typing_extensions==4.16.0 \
      attrs==26.1.0 \
      geomet==1.1.0 \
      SQLAlchemy==2.0.36
COPY results.py /runner/results.py
COPY nodetool-docker /usr/local/bin/nodetool-docker
ENV PYTHONPATH=/runner PYTHONDONTWRITEBYTECODE=1 SCYLLA_TEST_RUNNER=runpy
