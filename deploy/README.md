# Deploying across two machines

This is the split `README.md`'s "Target deployment" section describes: one
machine owns the data and faces the network, one or more others are pure
compute. Both run the same image -- `.github/workflows/publish.yml` builds it
once, from `main`, and pushes it to `ghcr.io/gferreiros/gavel`.

Neither compose file runs a reverse proxy. Ingress -- TLS, the public
hostname -- is whatever already terminates it in front of your network (a
VPS running Nginx Proxy Manager, a home router, anything); these files only
assume that thing can reach the web machine over Tailscale, and get out of
its way.

| File | Runs on |
|---|---|
| `compose.web.yml` + `.env` (from `.env.web.example`) | the data/web machine, once |
| `compose.worker.yml` + `.env` (from `.env.worker.example`) | each compute machine |

Watchtower runs on every machine, polls GHCR every five minutes, and
redeploys the labelled container in place when `main` has produced a new
image. That is the whole of "update yourselves" -- nothing on these machines
runs `git pull` or `cargo build`.

## First bring-up

Both machines need Tailscale up before either compose file will start --
both required env vars below point at a `100.x.y.z` address:

```bash
curl -fsSL https://tailscale.com/install.sh | sh
tailscale up
tailscale ip -4   # this machine's TAILSCALE_ADDR / half of the other's WEB_ADDR
```

On the web/data machine:

```bash
mkdir -p /opt/gavel && cd /opt/gavel
curl -fsSLO https://raw.githubusercontent.com/GFerreiroS/gavel/main/deploy/compose.web.yml
curl -fsSL https://raw.githubusercontent.com/GFerreiroS/gavel/main/deploy/.env.web.example -o .env
# edit .env: BLIZZARD_CLIENT_ID/SECRET, APP_CLUSTER_TOKEN, TAILSCALE_ADDR
docker compose -f compose.web.yml up -d
```

On each compute machine:

```bash
mkdir -p /opt/gavel && cd /opt/gavel
curl -fsSLO https://raw.githubusercontent.com/GFerreiroS/gavel/main/deploy/compose.worker.yml
curl -fsSL https://raw.githubusercontent.com/GFerreiroS/gavel/main/deploy/.env.worker.example -o .env
# edit .env: APP_CLUSTER_TOKEN (same value as the web machine), WEB_ADDR
docker compose -f compose.worker.yml up -d --scale worker=8
```

Then point your proxy (NPM's "Proxy Host", or equivalent) at
`http://<web machine's TAILSCALE_ADDR>:3000`, with forwarding for
`X-Forwarded-For` and `X-Forwarded-Proto` on -- `APP_TRUST_PROXY_HEADERS` is
already on in `compose.web.yml`, and needs both to be honest about who's
asking and whether the hop to them was HTTPS.

## Networking

`APP_CLUSTER_TOKEN` authenticates a worker; it does not make port 3001 safe
to publish. `compose.web.yml` binds both 3000 and 3001 to `${TAILSCALE_ADDR}`
-- never `0.0.0.0`, never a public interface -- so reaching either one
requires a key for this tailnet, not just line-of-sight on a shared network.
That is what actually keeps five bytes on 3001 from being the whole of
joining the cluster the way it was before `APP_CLUSTER_TOKEN` existed
(root `README.md` §10); the token is the second lock, this bind is the
first.

If ingress is a VPS: join it to the same tailnet and give NPM's proxy host
the web machine's `TAILSCALE_ADDR` as its target, not a LAN or public IP.
The VPS is then the only machine here with a public port open at all -- both
Proxmox VMs stay off the internet entirely.

## Changing how many workers, or which regions

- Worker count: re-run `docker compose -f compose.worker.yml up -d --scale
  worker=N` on that machine. Watchtower changes *what image* runs, never
  *how many* -- scaling stays a decision made here, on purpose.
- Regions collected, catalogue, retention: `/admin` on the running instance,
  or `.env`'s `APP_MARKET_REGIONS` and a restart. See the root `README.md`'s
  configuration table for the rest of the flags.

## Verifying the pieces independently

```bash
docker compose -f compose.web.yml logs -f web       # collector, cluster, HTTP
docker compose -f compose.web.yml logs -f watchtower # what it last checked/pulled
curl -s http://<web machine's TAILSCALE_ADDR>:3000/healthz  # liveness, no database
curl -s https://<your public hostname>/readyz                # through the proxy, 204 once analysis exists
```
