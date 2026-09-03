FROM ubuntu:24.04@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517

ARG DEBIAN_FRONTEND=noninteractive
ARG NODE_VERSION=22.19.0
ARG NODE_SHA256=0b2d9f564b6594222a62c82e1df2efe119dd4a4aff29644f4dd325bf360b6bcc
ARG BUN_VERSION=1.4.0
ARG BUN_SHA256=4b1a332ee861983eb93bcfe6f770fff94e3e31b2c388bdaea3c8ed35e58eed0e
ARG RUSTUP_VERSION=1.28.2
ARG RUSTUP_SHA256=e3853c5a252fca15252d07cb23a1bdd9377a8c6f3efa01531109281ae47f841c
ARG RUST_VERSION=1.97.1

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential ca-certificates curl git jq passwd pkg-config python3 rsync unzip xz-utils \
 && rm -rf /var/lib/apt/lists/*

RUN curl -fsSLO "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-arm64.tar.xz" \
 && echo "${NODE_SHA256}  node-v${NODE_VERSION}-linux-arm64.tar.xz" | sha256sum -c - \
 && tar -xJf "node-v${NODE_VERSION}-linux-arm64.tar.xz" -C /usr/local --strip-components=1 \
 && rm "node-v${NODE_VERSION}-linux-arm64.tar.xz" \
 && npm install --global "pnpm@11.7.0"

RUN curl -fsSL -o bun.zip "https://github.com/oven-sh/bun/releases/download/bun-v${BUN_VERSION}/bun-linux-aarch64.zip" \
 && echo "${BUN_SHA256}  bun.zip" | sha256sum -c - \
 && unzip -q bun.zip \
 && install bun-linux-aarch64/bun /usr/local/bin/bun \
 && ln -s /usr/local/bin/bun /usr/local/bin/bunx \
 && rm -rf bun.zip bun-linux-aarch64

ENV RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:/usr/local/bin:/usr/bin:/bin \
    PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright
RUN curl -fsSL -o rustup-init "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/aarch64-unknown-linux-gnu/rustup-init" \
 && echo "${RUSTUP_SHA256}  rustup-init" | sha256sum -c - \
 && chmod +x rustup-init \
 && ./rustup-init -y --no-modify-path --profile minimal --default-toolchain "${RUST_VERSION}" \
 && rm rustup-init

WORKDIR /opt/playwright
COPY web/package.json web/bun.lock ./
RUN bun install --frozen-lockfile \
 && ./node_modules/.bin/playwright-core install --with-deps chromium \
 && rm -rf /var/lib/apt/lists/*

RUN /usr/sbin/useradd --create-home --uid 10001 benchmark \
 && mkdir -p /bench /results \
 && chown -R benchmark:benchmark /bench /results /opt/cargo /opt/rustup /opt/ms-playwright /opt/playwright

COPY --chown=benchmark:benchmark benchmarks/run-linux.sh /usr/local/bin/run-tessivum-benchmarks
USER benchmark
WORKDIR /bench
ENTRYPOINT ["/usr/local/bin/run-tessivum-benchmarks"]
