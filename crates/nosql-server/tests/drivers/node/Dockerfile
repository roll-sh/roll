# The Node driver suite (cassandra-driver).
FROM node:22-slim
WORKDIR /suite
COPY package.json .
RUN npm install --no-audit --no-fund --omit=dev --silent
COPY suite.js .
ENTRYPOINT ["node", "/suite/suite.js"]
