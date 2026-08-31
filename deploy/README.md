# Deploying across two machines

This is the split `README.md`'s "Target deployment" section describes: one
machine owns the data and faces the network, one or more others are pure
compute. Both run the same image -- `.github/workflows/publish.yml` builds it
once, from `main`, and pushes it to `ghcr.io/gferreiros/gavel`.

| File | Runs on |
|---|---|
| `compose.web.yml` + `.env` (from `.env.web.example`) | the data/web machine, once |
| `compose.worker.yml` + `.env` (from `.env.worker.example`) | each compute machine |

Watchtower runs on every machine, polls GHCR every five minutes, and
redeploys the labelled container in place when `main` has produced a new
image. That is the whole of "update yourselves" -- nothing on these machines
runs `git pull` or `cargo build`.

## First bring-up

On the web/data machine:

```bash
mkdir -p /opt/gavel && cd /opt/gavel
curl -fsSLO https://raw.githubusercontent.com/GFerreiroS/gavel/main/deploy/compose.web.yml
curl -fsSLO https://raw.githubusercontent.com/GFerreiroS/gavel/main/deploy/Caddyfile
curl -fsSL https://raw.githubusercontent.com/GFerreiroS/gavel/main/deploy/.env.web.example -o .env
# edit .env: BLIZZARD_CLIENT_ID/SECRET, APP_CLUSTER_TOKEN, PRIVATE_ADDR, APP_DOMAIN
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

## Networking

`APP_CLUSTER_TOKEN` authenticates a worker; it does not make port 3001 safe
to publish. `compose.web.yml` binds it to `${PRIVATE_ADDR}` -- the web
machine's address on whatever network the compute machines are actually on
(a Proxmox internal bridge, a VPN, a VLAN) -- specifically so a firewall rule
is not the only thing standing between that port and the internet. Only 80
and 443 (Caddy) belong on the public interface.

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
curl -s https://<APP_DOMAIN>/healthz                 # liveness, no database
curl -s https://<APP_DOMAIN>/readyz                  # 204 once analysis exists
```
