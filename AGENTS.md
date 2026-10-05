# AGENTS.md

This file provides guidance to coding agents (Claude Code, Codex, pi) working in this repository.

rusti2 is the Cotab workspace's only server-side entry point to Cloudflare R2:
a tonic gRPC service (`rusti2.v1.ObjectStorage`, port 3002) called by
`cotab-api` and `indexer`. Cross-repository rules (addressing, proto
versioning, release flow, commit format) live in the workspace
[`../AGENTS.md`](../AGENTS.md); `README.md` covers the authorization model,
observability and SLO.

## Commands

Setup, `make dev` / `make test` and the `protoc` requirement are in
[`README.md`](README.md#running-locally). Fill in `local.env` before
`make dev`: the `REPLACE_ME` tokens copied from `example.env` are shorter than
24 characters, so the process exits at startup. Single test: `cargo test
<name>`; single integration binary: `cargo test --test authz`.

- CI (`.github/workflows/ci.yml`) runs `cargo test --locked --lib --bins` and
  `cargo test --locked --test '*'`, then on `main` pushes
  `ghcr.io/brojyf/rusti2:{latest,<sha>}` and triggers the Coolify deploy.
  It does **not** run `fmt`, `clippy` or `cargo deny`; run
  `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` locally.
- Tests never reach R2: the S3 client points at `https://example.invalid` or a
  local TCP stub, so they cover auth/policy/telemetry, not real object I/O.

## Architecture

Request path (`src/main.rs`), outermost layer first:

1. `HealthLayer` answers `GET /api/health` (the Docker `HEALTHCHECK`) before
   anything else runs. The server speaks HTTP/2 only, so probe it with
   `curl --http2-prior-knowledge`; a plain HTTP/1.1 `curl` fails.
2. `TraceLayer` + `telemetry::grpc_span` — one span per RPC that continues the
   inbound W3C `traceparent`; records only the RPC path, never headers or
   payloads.
3. `tonic_health` is mounted **without** auth; only `ObjectStorage` is wrapped
   in `InterceptedService` with `auth::ServiceTokenAuth`.
4. The interceptor resolves `authorization: Bearer <token>` to an
   `Arc<Caller>` and stores it in request extensions. Every auth failure
   returns the same `UNAUTHENTICATED "invalid service token"` message on
   purpose.
5. Each handler in `service.rs` calls `caller_of(&request)` (fails closed with
   `INTERNAL` if the interceptor is missing) and then `authorize(caller,
   Method, bucket, key)` before touching R2. `authorize` is the single place
   that does both the bucket/key checks and the policy check — new RPCs must
   call it, not split it. Other fields are validated per handler:
   `PresignPut` requires `content_type`, `UploadObject` does not.

Policy (`src/policy.rs`) is parsed once from `RUSTI2_CALLERS` (JSON) at
startup, and any problem stops the process (every case is a `PolicyError`
variant; `Config::from_env` also requires `CLOUDFLARE_ACCOUNT_ID`,
`R2_ACCESS_KEY_ID` and `R2_SECRET_ACCESS_KEY`). A caller is
allowed iff the method is in its grant **and** some scope matches
`bucket == scope.bucket && key.starts_with(scope.key_prefix)`. There is no
global bucket allowlist and no path normalization; a trailing `*` in a scope
is cosmetic. Adding a caller or widening a grant is an env change, not a
release.

`auth.rs` + `policy.rs` are the workspace's reference implementation of the
service-token pattern for Rust services (Go counterpart:
`cotab-api/internal/grpcapi/auth.go`); keep them consistent.

Handler specifics worth knowing:
- `UploadObject` is client-streaming: first message must be metadata, and it
  is authorized **before** any body chunk is read. The body is buffered in
  memory, capped at 64 MiB, then sent as one `PutObject`.
- `DownloadObject` streams 1 MiB chunks from a spawned task that is
  `.in_current_span()` so errors stay on the request's trace (covered by
  `tests/download_trace.rs`).
- `PresignPut` expiry: 0 → 900 s, otherwise clamped to 60–3600 s.
- `Delete` is idempotent (R2 succeeds on a missing key).
- R2 errors are logged with detail and returned as `INTERNAL "<op> failed"`,
  except a missing object in `Stat` / `Download`, which is `NOT_FOUND`.
  Denials are logged with the caller name, never the token.

Telemetry (`src/telemetry.rs`): behavior and env vars are in
[`README.md`](README.md#observability). In code: `main` logs any error from
`run()` through tracing, then calls `Shutdown::shutdown()`; dropping
`Shutdown` does not reliably flush, so every exit path must go through it.

## Test gotchas

- `tests/download_trace.rs` and `tests/telemetry_export.rs` install a global
  tracing subscriber, so each holds exactly one test in its own binary. Put a
  new test that needs a global subscriber in a new file.
- The proptests live in the `#[cfg(test)]` modules of `auth.rs` and
  `policy.rs` so they call the real private functions; don't move them to
  `tests/`, which can only see the public API.
