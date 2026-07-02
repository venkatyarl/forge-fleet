#!/usr/bin/env bash
# Install uv (if missing) then kimi-cli, on an Ubuntu fleet node. Idempotent.
set +e
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"

if ! command -v uv >/dev/null 2>&1; then
  echo ">> uv missing — installing"
  if command -v curl >/dev/null 2>&1; then
    curl -LsSf https://astral.sh/uv/install.sh | sh
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- https://astral.sh/uv/install.sh | sh
  else
    echo "!! no curl or wget; trying pip"
    python3 -m pip install --user --break-system-packages uv 2>/dev/null
  fi
  export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
fi

UV="$(command -v uv || echo "$HOME/.local/bin/uv")"
echo ">> uv = $UV"
"$UV" tool install kimi-cli --force 2>&1 | tail -3

echo "== RESULT =="
if [ -x "$HOME/.local/bin/kimi" ]; then
  echo "KIMI_OK $HOME/.local/bin/kimi"
  "$HOME/.local/bin/kimi" --version 2>&1 | head -1
else
  echo "KIMI_MISSING"
fi
