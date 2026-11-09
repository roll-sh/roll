# The ceph/s3-tests runner for scripts/test-s3-conformance.sh. Versions are pinned so results
# only change when roll (or the pinned s3-tests commit) does.
FROM python:3.12-slim
RUN pip install --no-cache-dir \
    boto3==1.43.117 botocore==1.43.117 munch==4.0.0 gevent==26.9.0 isodate==0.7.2 \
    requests==2.34.2 urllib3==2.8.0 pytz==2026.5 httplib2==0.32.0 lxml==6.1.3 PyYAML==6.0.3 \
    pytest==9.1.1 pytest-xdist==3.8.0
WORKDIR /work
