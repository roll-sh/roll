FROM node:22-alpine
WORKDIR /app
COPY package.json package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci --omit=dev
COPY main.js .
ENTRYPOINT ["node", "main.js"]
