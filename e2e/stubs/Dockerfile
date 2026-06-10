FROM node:22-bookworm-slim

WORKDIR /e2e

COPY package.json package-lock.json ./
RUN npm ci

COPY tsconfig.json ./
COPY fixtures ./fixtures
COPY stubs ./stubs

ENV AUDIO_DIR=/e2e/.fixtures/audio \
    DOWNLOADS_DIR=/downloads

EXPOSE 5030 5050

CMD ["npx", "tsx", "stubs/src/main.ts"]
