# Deployment

The gateway ships as a single static binary (`sms-gateway`) in a distroless,
non-root container image, published for `linux/amd64` and `linux/arm64`:

```
ghcr.io/alcelafranque/sms-api-gateway:<tag>
```

| Tag | Published on |
|---|---|
| `1.2.3`, `1.2`, `latest` | git tag `v1.2.3` (no `latest` for pre-releases) |
| `edge`, `sha-<short>` | every push to `main` |

It needs a PostgreSQL database (tested with PostgreSQL 18) and HTTP(S)
access to the device web UI.

## Building the image

The [`Containerfile`](../Containerfile) build stage always runs on the build
host's architecture and cross-compiles to the target with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) (zig is the C
compiler and linker for `ring`). A multi-arch build therefore needs no QEMU
and takes about as long as two native builds.

```sh
# Docker (BuildKit)
docker buildx build -f Containerfile --platform linux/amd64,linux/arm64 \
  -t registry.example/sms-gateway:dev --push .

# Podman / Buildah
podman build --platform linux/amd64,linux/arm64 \
  --manifest sms-gateway:dev -f Containerfile .
podman manifest push sms-gateway:dev docker://registry.example/sms-gateway:dev

# Single architecture, local
podman build -t sms-gateway -f Containerfile .
```

Build arguments: `RUST_VERSION` (default `1.100`), `ZIG_VERSION` with its
`ZIG_SHA256_AMD64` / `ZIG_SHA256_ARM64`, and `CARGO_ZIGBUILD_VERSION`.

The image runs as UID/GID `65532`, with `ENTRYPOINT ["/usr/local/bin/sms-gateway"]`,
`CMD ["serve"]` and working directory `/etc/sms-gateway`. It contains no
shell. CA certificates (for HTTPS webhooks) come from the base image.

## Configuration

Every setting of [`config.example.toml`](../config.example.toml) can be
given as an environment variable `SMS_GATEWAY__<SECTION>__<KEY>`, with
double underscores between levels. A configuration file is optional:
environment variables alone are enough, and override the file when both are
used.

| Setting | Variable |
|---|---|
| `database.url` | `SMS_GATEWAY__DATABASE__URL` |
| `modem.kind` | `SMS_GATEWAY__MODEM__KIND` (`tplink` or `mock`) |
| `modem.tplink.url` | `SMS_GATEWAY__MODEM__TPLINK__URL` |
| `modem.tplink.password` | `SMS_GATEWAY__MODEM__TPLINK__PASSWORD` |
| `metrics.listen` | `SMS_GATEWAY__METRICS__LISTEN`, e.g. `0.0.0.0:9100` |
| `log.format` | `SMS_GATEWAY__LOG__FORMAT` (`json` for log collectors) |
| `validation.allowed_prefixes` | `SMS_GATEWAY__VALIDATION__ALLOWED_PREFIXES='["+33"]'` |

Pass secrets (database URL, device password) through the environment, from
a secret store. The file, when used, is read from `./config.toml`, i.e.
`/etc/sms-gateway/config.toml` in the image, or from `--config <path>` /
`SMS_GATEWAY_CONFIG` (then it must exist).

## Docker Compose

[`compose.yaml`](../compose.yaml) runs PostgreSQL 18 and the gateway. The
gateway image is pulled from GHCR, or built from the `Containerfile` with
`docker compose build`.

```sh
cp .env.example .env     # set POSTGRES_PASSWORD, MODEM_URL, MODEM_PASSWORD
docker compose up -d     # podman compose works too
docker compose logs -f sms-gateway
```

The API listens on `127.0.0.1:8080` (`SMS_GATEWAY_BIND`, `SMS_GATEWAY_PORT`).
Database migrations run when the gateway starts. To change other settings,
add `SMS_GATEWAY__*` variables to the `environment` block, or uncomment the
`config.toml` mount.

**Without hardware**: set `MODEM_KIND=mock` in `.env`. Nothing is sent, and
the `/v1/dev/*` routes simulate inbound SMS and device faults.

The compose file has no container healthcheck: the image has neither shell
nor HTTP client. Check `GET /healthz` and `GET /readyz` from the host.

### First app

Apps (API clients) are managed with the CLI, against the same database:

```sh
docker compose run --rm sms-gateway app create ops --admin
docker compose run --rm sms-gateway app create billing \
  --webhook-url https://billing.internal/sms --keyword PAY
docker compose run --rm sms-gateway app list
```

`app create` prints the API key (and the webhook signing secret) once; store
them right away.

## Kubernetes

### One replica, `Recreate`

Run **exactly one** pod, and use `strategy: Recreate`:

- The device accepts a single logged-in admin session. A second gateway
  instance, even briefly during a rolling update, logs in and kicks the
  first one out, which then logs back in, and so on. Sends fail or end in
  `unknown`, and the inbox is polled twice.
- `RollingUpdate` starts the new pod before the old one stops: it causes
  exactly that overlap. `Recreate` stops the old pod (which releases its
  session) before starting the new one.

The outbound queue itself is safe with several instances (rows are claimed
with `FOR UPDATE SKIP LOCKED`), but the device session is not. Do not add a
HorizontalPodAutoscaler.

Likewise, do not use the device web UI while the gateway runs: logging in
there ends the gateway's session (and the reverse). Pause the gateway first:

```sh
curl -X POST -H "Authorization: Bearer $ADMIN_KEY" \
  "https://sms.example/v1/modem/pause?duration_secs=1800"
# ... use the web UI, log out ...
curl -X POST -H "Authorization: Bearer $ADMIN_KEY" https://sms.example/v1/modem/resume
```

The pause is stored in the database: it survives pod restarts.

### Probes

- `/healthz` always answers `200` while the process runs.
- `/readyz` answers `200` when the database answers **and** the device
  session is `ok` or `paused`. It answers `503` when the database is down,
  when the last device call failed (device unreachable, login refused...),
  and briefly at startup before the first device call (`idle`).

Recommendations:

- **Liveness: `/healthz`.** Never `/readyz`: a device outage would restart
  the pod in a loop, without fixing anything.
- **Readiness: `/healthz` too**, in most cases. With a single replica, a
  failing readiness probe removes the only endpoint from the Service, and
  clients cannot even queue messages (`POST /v1/messages` only needs the
  database; messages are sent once the device is back). Use `/readyz` for
  readiness only if clients should rather fail fast while the device is
  down.
- **Alert on `/readyz`** instead (blackbox exporter), or on the metrics
  `sms_modem_errors_total`, `sms_inbound_last_successful_poll_timestamp_seconds`,
  `sms_outbound_queue_depth` and `sms_modem_paused`.

If the database is unreachable at startup, `serve` exits with an error and
Kubernetes restarts it; the device is not contacted at startup.

### Example

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: sms-gateway
spec:
  replicas: 1                 # never more: single device session
  strategy:
    type: Recreate            # no overlap between old and new pod
  selector:
    matchLabels: { app.kubernetes.io/name: sms-gateway }
  template:
    metadata:
      labels: { app.kubernetes.io/name: sms-gateway }
    spec:
      # Above server.shutdown_timeout_secs (30): in-flight sends finish and
      # the device session is released.
      terminationGracePeriodSeconds: 45
      securityContext:
        runAsNonRoot: true
        runAsUser: 65532
        runAsGroup: 65532
        seccompProfile: { type: RuntimeDefault }
      containers:
        - name: sms-gateway
          image: ghcr.io/alcelafranque/sms-api-gateway:1.0.0
          args: ["serve"]
          env:
            - name: SMS_GATEWAY__DATABASE__URL
              valueFrom:
                secretKeyRef: { name: sms-db-app, key: uri }   # CloudNativePG
            - name: SMS_GATEWAY__MODEM__KIND
              value: tplink
            - name: SMS_GATEWAY__MODEM__TPLINK__URL
              value: http://192.168.254.1
            - name: SMS_GATEWAY__MODEM__TPLINK__PASSWORD
              valueFrom:
                secretKeyRef: { name: sms-gateway, key: modem-password }
            - name: SMS_GATEWAY__METRICS__LISTEN
              value: 0.0.0.0:9100
            - name: SMS_GATEWAY__LOG__FORMAT
              value: json
          ports:
            - { name: http, containerPort: 8080 }
            - { name: metrics, containerPort: 9100 }
          livenessProbe:
            httpGet: { path: /healthz, port: http }
            periodSeconds: 20
            failureThreshold: 3
          readinessProbe:
            httpGet: { path: /healthz, port: http }
            periodSeconds: 10
          resources:
            requests: { cpu: 10m, memory: 64Mi }
            limits: { memory: 128Mi }
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities: { drop: ["ALL"] }
```

Add a Service for ports `http` and `metrics`, a `PodMonitor` or
`ServiceMonitor` on `metrics`, and an `HTTPRoute` (Envoy Gateway) to `http`.
The dedicated metrics listener is unauthenticated: do not route it outside
the cluster. Without `metrics.listen`, `/metrics` is served on the main port
to admin apps only.

The process is mostly idle: negligible CPU and about 35 MiB of resident
memory (measured on a 16-core host; the async runtime starts one worker
thread per core). The memory limit leaves room for webhook bursts
(`webhooks.concurrency`) and the database pool (`database.max_connections`).

### Database

[CloudNativePG](https://cloudnative-pg.io/) creates a `<cluster>-app`
secret whose `uri` key (`postgresql://app:...@<cluster>-rw.<ns>:5432/app`)
can be used as is for `SMS_GATEWAY__DATABASE__URL`. The connection
negotiates TLS (`sslmode=prefer` by default); to verify the server
certificate, mount the `<cluster>-ca` secret and append
`?sslmode=verify-full&sslrootcert=/path/to/ca.crt`. Keep
`database.max_connections` (10) well below the cluster's `max_connections`.

Migrations run at startup (`database.migrate_on_start`, default `true`); the
role needs to create tables in its database, which the CloudNativePG owner
role can. To run them as a separate step instead, set
`SMS_GATEWAY__DATABASE__MIGRATE_ON_START=false` and add an init container
(or an Argo CD `PreSync` hook Job) with the same image, the same database
URL and `args: ["migrate"]`; it needs no modem settings.

Create the first app from the running pod (the CLI reads the same
environment):

```sh
kubectl exec deploy/sms-gateway -- /usr/local/bin/sms-gateway app create ops --admin
```

### Device reachability

Pods must reach the device web UI (`modem.tplink.url`): it usually sits on a
LAN behind a node, reached through the node's routing and pod egress NAT.
Check it from a pod on the node where the gateway can be scheduled, e.g.
with a debug container:
`kubectl debug -it deploy/sms-gateway --image=busybox -- wget -qO- http://192.168.254.1`.
If only some nodes can reach the device, pin the pod with a `nodeSelector`
or node affinity. With egress NetworkPolicies, allow the device IP and port
(80 or 443), PostgreSQL, the webhook targets, and DNS.

Provide the device password and the database URL as Secrets (e.g. from
External Secrets). They are read at startup only: restart the pod after a
rotation.
