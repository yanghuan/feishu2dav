# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml ./
COPY src ./src

RUN cargo build --release \
    && strip target/release/feishu2dav \
    && /app/target/release/feishu2dav --help | grep -q "WebDAV"

FROM debian:bookworm-slim

LABEL org.opencontainers.image.source="https://github.com/yanghuan/feishu2dav" \
      org.opencontainers.image.description="Feishu (Lark) Cloud Drive WebDAV gateway using App ID/Secret + User OAuth" \
      org.opencontainers.image.licenses="MIT"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 feishu2dav \
    && useradd --uid 10001 --gid 10001 --create-home --shell /usr/sbin/nologin feishu2dav \
    && mkdir -p /data \
    && chown -R feishu2dav:feishu2dav /data

COPY --from=builder /app/target/release/feishu2dav /usr/local/bin/feishu2dav

USER feishu2dav
WORKDIR /data

RUN /usr/local/bin/feishu2dav --help | grep -q "FEISHU_APP_ID"

ENV FEISHU2DAV_LISTEN=0.0.0.0:4918 \
    FEISHU2DAV_TOKEN_FILE=/data/feishu2dav-tokens.json

VOLUME ["/data"]
EXPOSE 4918

ENTRYPOINT ["/usr/local/bin/feishu2dav"]
CMD ["serve"]
