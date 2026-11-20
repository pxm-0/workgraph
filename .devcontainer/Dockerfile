# The one toolchain image: devcontainer, compose dev stack and CI all build this file.
# Base images are pinned by digest; bump the tag and digest together, and the rust tag with
# rust-toolchain.toml.
FROM node:22.23.5-trixie-slim@sha256:68c76cfad89921b927eae28ba21407475e29d0ce450afcdbe70c1532db042b64 AS node

FROM rust:1.100.0-slim-trixie@sha256:6f14b2b4b278ec0d50aa1d69541c304ce0f95e0f6268487aa9924af1c2e9aebe

# git and python3 for the wg tests and the Haruspex CLI, jq for the finalize fixture, procps for
# the kill(1) that `wg check` uses to stop a timed-out process group.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git jq procps python3 \
 && rm -rf /var/lib/apt/lists/*

# node and npm because the finalize fixture's check is `npm test`.
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm

# thumbv7em-none-eabihf ships no std, so building wg-factory for it proves the crate never names std.
RUN rustup component add rustfmt clippy llvm-tools-preview \
 && rustup target add thumbv7em-none-eabihf \
 && cargo install --locked cargo-llvm-cov@0.9.1 \
 && rm -rf /usr/local/cargo/registry /usr/local/cargo/git

# Tests create throwaway repos; the bind-mounted checkout is owned by the host user.
RUN git config --system --add safe.directory /workspace
WORKDIR /workspace
