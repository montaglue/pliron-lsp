# The VS Code extension's integration tests in Linux, on a virtual display
# (xvfb), so that no VS Code window opens on the host. Use it through
# `cargo xtask test-vscode [--bundled]`, which mounts the repository
# read-only and keeps builds and downloads in Docker volumes.
FROM rust:1-bookworm

# Node.js, from the official image.
COPY --from=node:22-bookworm /usr/local/bin/node /usr/local/bin/
COPY --from=node:22-bookworm /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx

# A virtual display, and the libraries VS Code (Electron) needs.
RUN apt-get update && apt-get install -y --no-install-recommends \
      xvfb xauth rsync \
      libasound2 libatk-bridge2.0-0 libatk1.0-0 libcups2 libdrm2 libgbm1 \
      libgtk-3-0 libnss3 libsecret-1-0 libx11-xcb1 libxkbfile1 libxshmfence1 libxss1 \
 && rm -rf /var/lib/apt/lists/*

# Builds go to a volume (not the host's target/); the tests use that
# server. VS Code runs as root here, which needs Chromium's sandbox off.
ENV CARGO_TARGET_DIR=/cargo-target \
    PLIRON_LSP_SERVER=/cargo-target/debug/pliron-lsp \
    PLIRON_TEST_NO_SANDBOX=1
WORKDIR /work
