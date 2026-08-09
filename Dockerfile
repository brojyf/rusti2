# syntax=docker/dockerfile:1.7
FROM rust:1.97-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-client protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY .cargo ./.cargo
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN --mount=type=secret,id=proto_ssh_key \
    printf '%s\n' '[ssh.github.com]:443 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl' > /tmp/github-known-hosts \
    && git config --global core.sshCommand "ssh -i /run/secrets/proto_ssh_key -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=/tmp/github-known-hosts" \
    && git config --global url."ssh://git@ssh.github.com:443/".insteadOf "https://github.com/" \
    && cargo build --locked --release \
    && git config --global --unset-all url."ssh://git@ssh.github.com:443/".insteadOf \
    && git config --global --unset-all core.sshCommand \
    && rm /tmp/github-known-hosts

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/rusti2 /usr/local/bin/rusti2

ENV RUSTI2_BIND_ADDR=0.0.0.0:3002

EXPOSE 3002

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD curl --fail --silent --http2-prior-knowledge http://127.0.0.1:3002/api/health || exit 1

USER 10001:10001

ENTRYPOINT ["rusti2"]
