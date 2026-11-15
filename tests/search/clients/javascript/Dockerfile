FROM node:22-slim
WORKDIR /app
COPY package.json .
RUN --mount=type=cache,target=/root/.npm npm install --no-audit --no-fund
COPY *.mjs ./
ENTRYPOINT ["node"]
CMD ["es_client.mjs"]
