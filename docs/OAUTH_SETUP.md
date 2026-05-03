# OAuth Setup — Claude / ChatGPT / Kimi

This guide walks the operator through enabling OAuth-based authentication for the three providers.

## Claude (Anthropic Pro/Team subscription)

### Installation
```bash
npm install -g @anthropic-ai/claude-code
```

### Login (Browser OAuth)
```bash
claude
```
This opens a browser window for OAuth authentication. Upon success, credentials are stored at `~/.claude/.credentials.json`.

### Import to Fleet
```bash
ff oauth import claude && ff oauth distribute claude --yes && ff oauth probe claude
```

### Expected Result
- `ff oauth status` shows `codex` → `yes` in SECRETS column
- `ff oauth probe claude` returns `authenticated` with token expiry

### Notes
- Claude's `.credentials.json` uses flat structure: `{"accessToken": "..."}` which works with current import logic.
- No known bugs with Claude import.

## ChatGPT (OpenAI Plus/Team via codex CLI)

### Installation
```bash
npm install -g @openai/codex
```

### Login (Browser OAuth)
```bash
codex login
```
This opens a browser window for OAuth authentication. Upon success, credentials are stored at `~/.codex/auth.json`.

### Import to Fleet
```bash
ff oauth import codex && ff oauth distribute codex --yes && ff oauth probe codex
```

### ⚠️ Known Bug
The current `ff oauth import codex` **fails** because the `auth.json` structure is nested:
```json
{
  "tokens": {
    "access_token": "eyJhbGci...",
    "id_token": "eyJhbGci...",
    "refresh_token": "rt_..."
  }
}
```

The import logic only checks flat keys (`access_token`, `accessToken`, `token`) at the root level, not `tokens.access_token`.

### Workaround Until Fixed
1. Manually extract the token:
   ```bash
   TOKEN=$(jq -r '.tokens.access_token' ~/.codex/auth.json)
   ff secrets set openai.oauth_token "$TOKEN"
   ```
2. Then distribute:
   ```bash
   ff oauth distribute codex --yes
   ```

### Status
- See `/tmp/oauth-codex-e2e.md` for full e2e test results.
- Fix required in `crates/ff-agent/src/oauth_distributor.rs` to add `tokens.access_token` to the token_fields list.

## Kimi (Moonshot Pro subscription)

### Installation
```bash
pip install moonshot-cli
# or follow vendor instructions at https://moonshot.ai
```

### Login (Browser OAuth)
```bash
kimi login
```
This opens a browser window for OAuth authentication. Upon success, credentials are stored at `~/.moonshot/auth.json`.

### Import to Fleet
```bash
ff oauth import kimi && ff oauth distribute kimi --yes && ff oauth probe kimi
```

### Expected Result
- `ff oauth status` shows `kimi` → `yes` in SECRETS column
- `ff oauth probe kimi` returns `authenticated` with token expiry

### Notes
- Kimi's auth.json structure is similar to codex — may have the same nested `tokens.*` issue.
- Verify the actual structure of `~/.moonshot/auth.json` before using.
- If import fails, use the same workaround as codex: manually extract `tokens.access_token` and set via `ff secrets set`.

## Troubleshooting

### 401 Unauthorized from probe
**Symptom:** `ff oauth probe <provider>` returns `401 Unauthorized` or `unauthorized`.

**Cause:** Token has expired (typically 24h for access tokens).

**Fix:**
1. Re-login via the vendor CLI:
   ```bash
   # For Claude
   claude logout && claude
   
   # For Codex
   codex logout && codex login
   
   # For Kimi
   kimi logout && kimi
   ```
2. Re-import:
   ```bash
   ff oauth import <provider>
   ```

### Missing credential file
**Symptom:** `ff oauth status` shows `missing` for CRED FILE.

**Cause:** User hasn't logged in yet, or login failed.

**Fix:**
1. Run the vendor CLI login command (see "Login" sections above).
2. Verify file exists:
   ```bash
   ls -la ~/.claude/.credentials.json  # Claude
   ls -la ~/.codex/auth.json          # Codex
   ls -la ~/.moonshot/auth.json       # Kimi
   ```
3. Re-run `ff oauth status`.

### Distribute task stuck or missing
**Symptom:** `ff oauth distribute <provider> --yes` says `enqueued 0 task(s)` or tasks don't appear in `ff defer list`.

**Cause:** Import failed (no token to distribute), or task was already processed.

**Fix:**
1. Check import success:
   ```bash
   ff oauth status
   ```
2. If import failed, see the "Known Bug" section for codex/kimi.
3. List deferred tasks:
   ```bash
   ff defer list | grep <provider>
   ```
4. List all tasks:
   ```bash
   ff tasks list | grep <provider>
   ```

### Import fails with "no token field found"
**Symptom:** `✗ <provider>: no token field found in <path> (tried [...]); the cred file shape may have changed`

**Cause:** The vendor CLI changed their credential file structure (e.g., nested `tokens.*` instead of flat).

**Fix:**
1. Inspect the cred file:
   ```bash
   cat ~/.codex/auth.json | jq '.'
   ```
2. Manually extract the token:
   ```bash
   TOKEN=$(jq -r '.tokens.access_token' ~/.codex/auth.json)
   ff secrets set openai.oauth_token "$TOKEN"
   ```
3. Report the new structure to the team for code fix in `oauth_distributor.rs`.

### TOS warning about distributing tokens
**Symptom:** `! TOS reminder: distributing one subscription's OAuth token to multiple machines is grey-area...`

**Meaning:** This is a compliance warning, not an error. Distributing one Pro/Plus account's token across N fleet nodes may violate vendor TOS.

**Options:**
1. **Compliant:** Use per-node logins (skip `ff oauth distribute`), each node logs in separately.
2. **Risk-aware:** Continue with distribution, understanding potential TOS violation.
3. **Enterprise:** Use team/enterprise accounts that explicitly allow multi-node usage.
