# Testcontainers for Node (Cassandra and ScyllaDB modules) with the roll image swapped in.
FROM node:22-slim
WORKDIR /tc
COPY package.json .
RUN npm install --no-audit --no-fund --silent
COPY check.js .
ENTRYPOINT ["node", "/tc/check.js"]
