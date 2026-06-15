# SearXNG Web-Grounding Backend

`ff research` grounds each sub-agent with live web results so factual sub-questions
cite current sources instead of model memory. The grounding chain in
`crates/ff-agent/src/tools/web_search.rs::fetch_web_context` tries backends in
order of reliability:

1. **SearXNG** (this doc) — a self-hosted [metasearch](https://docs.searxng.org/)
   instance on a fleet node, queried key-free via its JSON API. Primary when
   configured. Fleet-native, so it sidesteps the per-IP scraping blocks that wall
   direct DuckDuckGo access.
2. **DuckDuckGo** HTML scrape — works when DDG isn't throttling the leader's IP.
3. **Wikipedia** `list=search` — narrow coverage, but never IP-blocked.

SearXNG is **opt-in and zero-regression**: if the `searxng.url` secret is unset,
unreachable, or returns nothing, grounding silently falls through to DDG →
Wikipedia. Setting it up never breaks research; it only makes grounding more
robust.

## Why SearXNG

DuckDuckGo intermittently hard-blocks the leader with an HTTP-202 CAPTCHA
("select all squares containing a duck") that no retry clears — see the fix chain
PRs #338–#342. A fleet-hosted SearXNG removes the external-scraping dependency
entirely, fitting the "fleet replaces cloud subscriptions" model.

## Canonical deployment

- **Host:** sophie (Linux, low load). Any fleet member with Docker works.
- **Port:** `58080` (host) → `8080` (container). 5-digit, registered in
  `port_registry` (`ff ports list` → `web_service` / `searxng`).
- **Secret:** `searxng.url` = `http://<host-ip>:58080` (read by
  `fleet_info::fetch_secret("searxng.url")` in `research.rs`).

### 1. Config — JSON API + no bot-limiter

The default image returns **HTTP 403 for `format=json`** unless the `json` output
format is enabled, and its bot-limiter throttles programmatic calls. Write a
minimal `~/searxng/settings.yml` (merges over built-in defaults via
`use_default_settings`):

```yaml
use_default_settings: true
server:
  secret_key: "<openssl rand -hex 32>"
  limiter: false        # don't throttle our own JSON calls
  image_proxy: true
search:
  formats:
    - html
    - json              # <-- without this, format=json -> HTTP 403
```

### 2. Run the container

Restart policy `unless-stopped` so it survives reboots (no systemd unit needed):

```bash
ff ssh sophie "docker run -d --name searxng --restart unless-stopped \
  -p 58080:8080 -v ~/searxng:/etc/searxng:rw \
  docker.io/searxng/searxng:latest"
```

### 3. Register the port (DB-first; `ff ports` has no `add` verb yet)

`port_registry` is migration-seeded (V37) but operator SQL edits are preserved
across upgrades:

```sql
INSERT INTO port_registry (port, service, kind, description, exposed_on, scope,
                           managed_by, status, metadata)
VALUES (58080, 'searxng', 'web_service',
        'Self-hosted SearXNG metasearch — primary research grounding backend.',
        'sophie', 'lan', 'docker (restart=unless-stopped)', 'active',
        '{"container":"searxng","image":"searxng/searxng:latest","container_port":8080}'::jsonb)
ON CONFLICT (port) DO UPDATE SET metadata = EXCLUDED.metadata, updated_at = now();
```

> A future `ff ports add` verb would replace this raw SQL (backlog).

### 4. Point research at it

```bash
ff secrets set searxng.url "http://192.168.5.103:58080" \
  --description "Self-hosted SearXNG metasearch (sophie) — primary research grounding backend."
```

## Verify

```bash
# JSON API returns results (run from the leader, over the LAN):
curl -s 'http://192.168.5.103:58080/search?q=vllm+paged+attention&format=json' | jq '.results | length'
# -> a positive number, with {url,title,content} entries.

ff ports list | grep searxng          # registered
ff secrets get searxng.url            # set
```

A successful `ff research "<query>"` then grounds its sub-agents via SearXNG;
upstream-engine rate-limit warnings in `docker logs searxng` are normal (it
aggregates many engines and returns whatever succeeds).

## Rollback

Fully reversible — research falls back to DDG/Wikipedia immediately:

```bash
ff secrets delete searxng.url
ff ssh sophie "docker rm -f searxng"
# optionally: DELETE FROM port_registry WHERE port = 58080;
```
