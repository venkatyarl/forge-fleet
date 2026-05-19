# TB.4 — Shared `~/models` over NFS on Thunderbolt

**Status:** ready, needs operator sudo on Taylor.

## Goal

Make Taylor's `~/models` directory available read-only over Thunderbolt
to James so both can load the same GGUF/MLX weights without each
downloading their own copy. Free up James's 64 GB for inference, not
duplicate model storage.

## Steps

1. **Configure macOS NFS exports on Taylor**

`/etc/exports` (root-owned):

```
/Users/venkat/models -ro -alldirs -network 10.44.0.0 -mask 255.255.255.0
```

```bash
sudo nfsd restart
```

Verify:

```bash
showmount -e 10.44.0.1
```

2. **Mount on James** (also macOS):

```bash
ssh james "sudo mkdir -p /Volumes/taylor-models && \
  sudo mount -t nfs -o resvport,ro,nolocks 10.44.0.1:/Users/venkat/models /Volumes/taylor-models"
```

3. **Persist via launchd on James** so it auto-mounts at boot:

`~/Library/LaunchAgents/com.forgefleet.nfs-taylor-models.plist` →
calls the mount command at startup; verify with `launchctl bootstrap`.

4. **Wire into model deployments**

For each James deployment that wants Taylor's model:

```bash
ff model load <catalog-id> --node james --port 55003 \
  --model-path /Volumes/taylor-models/<dir>
```

The `model_path` column in `fleet_model_library` should be the NFS
mount path on James's side.

## Risks

- **NFS over TB is unbuffered** — sequential reads sustain ~15 Gbps in
  testing, but random small reads (which inference servers do at load
  time) can stutter. Pre-cache by `cat <model-file> > /dev/null` before
  starting the server.
- **Read-only is intentional** — James must not write into Taylor's
  models dir. Downloads still happen on Taylor; James only consumes.
- **macOS NFS handle stability** — after sleep/wake cycles the mount
  can go stale; the launchd plist should `umount && mount` on every
  boot rather than expecting persistence.

## Alternative considered

SMB/AFP — rejected. NFS is faster and has the lowest per-file overhead
for the GGUF mmap pattern.
