FROM python:3.13-slim
WORKDIR /app
COPY requirements.txt .
RUN --mount=type=cache,target=/root/.cache/pip pip install -r requirements.txt
COPY tasks.py main.py ./
ENTRYPOINT ["python", "-u", "main.py"]
