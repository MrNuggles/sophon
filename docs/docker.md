# Connecting Sophon to Docker

The Docker tab lists containers on your servers (running and stopped), their
published ports, and which server they're on, and can start / stop / restart
them — live. It's an **optional feature**, off in the default build.

## Build with the Docker tab

```bash
cargo build --release --features docker
```

Without `--features docker`, Sophon contains no Docker or network code at all.

## Set up a socket proxy on each server (required)

**Never expose the raw Docker socket over TCP.** The Docker API is
root-equivalent: anything that can reach it can take over the host. Instead, run
a small **filtered proxy** that exposes only what Sophon needs, on your LAN.

On each server, with Docker Compose:

```yaml
# docker-compose.yml
services:
  sophon-proxy:
    image: tecnativa/docker-socket-proxy
    container_name: sophon-proxy
    environment:
      CONTAINERS: 1        # list containers
      POST: 1              # allow start / stop / restart
      # everything else stays 0 (denied): EXEC, IMAGES, VOLUMES, INFO, AUTH, ...
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
    ports:
      - "2375:2375"        # bind to the LAN; firewall to Sophon's machine
    restart: unless-stopped
```

```bash
docker compose up -d
```

Then, in Sophon's **Docker tab**, add the server: a name and `host:port`
(e.g. `10.0.0.5:2375`). Sophon starts listing containers within a couple of
seconds and polls live while the tab is open.

### Locking it down

- **Firewall the proxy port** to Sophon's machine only. Anything that can reach
  `:2375` can start/stop your containers.
- Keep it on a **trusted LAN**. This uses plain HTTP (no encryption); it is not
  meant to cross the public internet. For remote access, tunnel over SSH or a
  VPN, or use Docker's native TLS.
- `CONTAINERS=1 POST=1` is the minimal allowlist. Leaving `EXEC`, `IMAGES`,
  `VOLUMES` etc. at `0` means even a compromised Sophon can't run commands in
  containers, build images, or touch volumes through the proxy.

## What Sophon uses

Only three Engine API endpoints, all through the proxy:
`GET /containers/json?all=1`, and `POST /containers/{id}/{start,stop,restart}`.
