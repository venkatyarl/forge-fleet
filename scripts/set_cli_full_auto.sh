#!/usr/bin/env bash
# Set codex / kimi / claude to full-auto (no approval prompts) on THIS node.
# Idempotent and CONSERVATIVE: only edits a config that already exists (never
# creates a half-broken one), backs up before first change, and skips a CLI
# whose config is absent. Safe to run repeatedly / fleet-wide.
set +e
ts=$(date +%Y%m%d-%H%M%S)
host=$(hostname -s 2>/dev/null || hostname 2>/dev/null || echo "?")
pre() { printf '%s: ' "$host"; }

# ---- kimi: ~/.kimi/config.toml  →  default_yolo = true ----
K="$HOME/.kimi/config.toml"
if [ -f "$K" ]; then
  if grep -qE '^\s*default_yolo\s*=\s*false' "$K"; then
    cp "$K" "$K.bak-$ts"
    sed -i.tmp -E 's/^\s*default_yolo\s*=\s*false/default_yolo = true/' "$K" && rm -f "$K.tmp"
    pre; echo "kimi: default_yolo -> true"
  elif grep -qE '^\s*default_yolo\s*=\s*true' "$K"; then
    pre; echo "kimi: already true"
  else
    cp "$K" "$K.bak-$ts"; printf '\ndefault_yolo = true\n' >> "$K"
    pre; echo "kimi: appended default_yolo = true"
  fi
else
  pre; echo "kimi: no config (skip)"
fi

# ---- codex + claude via python (TOML top-level insert / JSON key set) ----
python3 - "$ts" <<'PY'
import os, sys, json, re, shutil
ts = sys.argv[1]
home = os.path.expanduser("~")
host = os.uname().nodename.split(".")[0]
def say(m): print(f"{host}: {m}")

# codex: approval_policy="never" + sandbox_mode="danger-full-access", inserted
# BEFORE the first [table] (top-level TOML keys must precede any section).
cx = os.path.join(home, ".codex", "config.toml")
if os.path.isfile(cx):
    lines = open(cx).read().splitlines()
    have = lambda k: any(re.match(rf'\s*{k}\s*=', l) for l in lines)
    ap, sb = have("approval_policy"), have("sandbox_mode")
    if ap and sb:
        say("codex: already full-auto")
    else:
        shutil.copyfile(cx, f"{cx}.bak-{ts}")
        idx = next((i for i,l in enumerate(lines) if l.lstrip().startswith('[')), len(lines))
        ins = []
        if not ap: ins.append('approval_policy = "never"')
        if not sb: ins.append('sandbox_mode = "danger-full-access"')
        lines[idx:idx] = ins + ['']
        open(cx, "w").write("\n".join(lines) + "\n")
        say(f"codex: set {', '.join(ins)}")
else:
    say("codex: no config (skip)")

# claude: permissions.defaultMode = bypassPermissions (+ mirror config.permission_mode).
cj = os.path.join(home, ".claude", "settings.json")
if os.path.isfile(cj):
    try:
        d = json.load(open(cj))
    except Exception as e:
        say(f"claude: unreadable settings.json ({e}) — skip"); d = None
    if d is not None:
        perms = d.setdefault("permissions", {})
        changed = False
        if perms.get("defaultMode") != "bypassPermissions":
            perms["defaultMode"] = "bypassPermissions"; changed = True
        if isinstance(d.get("config"), dict) and d["config"].get("permission_mode") != "bypassPermissions":
            d["config"]["permission_mode"] = "bypassPermissions"; changed = True
        if changed:
            shutil.copyfile(cj, f"{cj}.bak-{ts}")
            json.dump(d, open(cj, "w"), indent=2)
            say("claude: defaultMode -> bypassPermissions")
        else:
            say("claude: already bypass")
else:
    say("claude: no config (skip)")
PY
