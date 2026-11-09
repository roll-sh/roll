"""Celery app and tasks shared by the scenario driver (main.py) and the worker subprocess."""

import os

from celery import Celery

URL = os.environ.get("AMQP_URL", "amqp://guest:guest@localhost:5672/%2F")
QUEUE = os.environ.get("CELERY_QTEST_QUEUE", "celery-qtest")

app = Celery("tasks", broker=URL.replace("amqp://", "pyamqp://", 1), backend="rpc://")
app.conf.update(
    task_default_queue=QUEUE,
    worker_prefetch_multiplier=1,
    broker_connection_retry_on_startup=True,
    broker_connection_max_retries=3,
    result_expires=300,
)


@app.task
def add(x, y):
    return x + y


@app.task
def mul(x, y):
    return x * y


@app.task(bind=True, max_retries=5)
def flaky(self):
    if self.request.retries < 2:
        raise self.retry(exc=RuntimeError(f"attempt {self.request.retries}"), countdown=0.2)
    return self.request.retries


@app.task
def fail(msg):
    raise ValueError(msg)


@app.task(acks_late=True)
def late(x):
    return x * 2
