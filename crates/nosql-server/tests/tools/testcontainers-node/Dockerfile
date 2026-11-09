# Testcontainers for Node (Cassandra and ScyllaDB modules) with the roll image swapped in.
FROM node:22-slim@sha256:5ec23b7c622b00547753bc6ffd78085dd868a68aeb41c8e0a1418f033e13d96d
WORKDIR /tc
COPY package.json .
RUN npm install --no-audit --no-fund --silent
COPY check.js .
ENTRYPOINT ["node", "/tc/check.js"]
