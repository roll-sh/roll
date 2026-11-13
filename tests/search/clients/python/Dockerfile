FROM python:3.12-slim
WORKDIR /app
COPY requirements.txt .
RUN --mount=type=cache,target=/root/.cache/pip pip install -r requirements.txt
COPY *.py ./
ENTRYPOINT ["python", "-u"]
CMD ["es_client.py"]
