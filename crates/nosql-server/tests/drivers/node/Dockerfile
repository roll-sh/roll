# The Node driver suite (cassandra-driver).
FROM node:22-slim@sha256:5ec23b7c622b00547753bc6ffd78085dd868a68aeb41c8e0a1418f033e13d96d
WORKDIR /suite
COPY package.json .
RUN npm install --no-audit --no-fund --omit=dev --silent
COPY suite.js .
ENTRYPOINT ["node", "/suite/suite.js"]
