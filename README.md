# rusti2

The only server-side entry point to Cloudflare R2. Other services never hold R2
credentials; they call `rusti2.v1.ObjectStorage` over gRPC and rusti2 decides
what they are allowed to touch.

## Authorization model

rusti2 holds broad R2 credentials on purpose; `RUSTI2_CALLERS` is what narrows
them. Each caller entry names a service, its bearer token, the methods it may
invoke, and the `bucket/key-prefix` scopes it may invoke them on. A request
outside that grant fails `PERMISSION_DENIED`; an unknown or absent token fails
`UNAUTHENTICATED`, and neither message says which.

There is no ambient access: reaching rusti2 over the network does not entitle a
caller to another service's objects. The process refuses to start if
`RUSTI2_CALLERS` is missing, malformed, or grants a caller nothing.

Grant the narrowest set that still works. `cotab-api` hands out upload URLs and
cleans up replaced objects but never moves bytes; `indexer` moves bytes
but never presigns.

## Dependencies

| Component | Why |
|---|---|
| Cloudflare R2 | The object store this service fronts. |
| `otel-collector` | Logs and traces over OTLP HTTP on 18001. |

No PostgreSQL. No queues. It never calls the dead-letter pipeline — that is for
queue consumers only.

Inbound: `cotab-api` and `indexer`.

## Running locally

```bash
cp example.env local.env   # then fill it in
make dev                   # cargo run
```

`local.env` is loaded by the Makefile and ignored by Git. Every variable,
including the `RUSTI2_CALLERS` policy format, is documented in
[`example.env`](example.env). Generate tokens with `openssl rand -hex 32`.

Port: `3002` (gRPC).

The gRPC types come from the tagged `cotab-proto` dependency. This
repository does not keep a local `.proto` or generated-code copy. Building
requires `protoc` because the shared crate compiles its sources at build time.

```bash
make test   # cargo test --all-targets, includes the proptest suite
```

## Observability

Set `OTEL_SERVICE_NAME=rusti2` and `OTEL_EXPORTER_OTLP_ENDPOINT` to the private
Collector's HTTP receiver (`http://otel-collector:18001`, or its deployed
network alias). Leave the endpoint empty to disable export locally. Grafana
credentials belong only in the Collector environment.

gRPC spans inherit inbound W3C `traceparent`, including streaming responses.
RPC failures are marked as errors. Structured logs are exported alongside
traces; the tracing layer records the RPC path without request headers or
payloads. Exporter diagnostics remain local to avoid recursive exports.
Exports are batched and best-effort. SIGTERM drains the server and flushes
telemetry before exit.

## SLO

| SLI | Target |
|---|---|
| gRPC availability | 99.9% |
| Unresolved archived failures across the platform (`ops.failed_messages where resolved_at is null`) | **always 0** |

The first row is a **trend**: measured over a window, and a bad hour is a
capacity conversation. The second is a platform-wide **incident** signal: one
unresolved row means a real message could not be processed and is waiting for a
human. There is no threshold to tune and no acceptable non-zero value.

Run 2 replicas: callers use `dns:///` targets with `round_robin`, which does
nothing useful against a single backend.

## Conventions

See the root [`AGENTS.md`](../AGENTS.md). `src/auth.rs` and `src/policy.rs` are
the reference implementation of the service-token pattern for the Rust
services; `cotab-api/internal/grpcapi/auth.go` is the Go one.
