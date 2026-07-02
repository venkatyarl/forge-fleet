#!/usr/bin/env bash
# Probe codex / claude / kimi on THIS host: resolve path, version, and fire a
# tiny real prompt to confirm login/auth + a live response.
# Runs under a login shell so PATH includes /opt/homebrew/bin + ~/.local/bin.
# Emits one line per CLI:  HOST|CLI|VERDICT|PATH|VERSION|DETAIL

host=$(hostname -s 2>/dev/null || hostname 2>/dev/null || echo "?")

# Portable timeout (macOS lacks `timeout`): perl alarm wrapper.
run_to() {
  local secs=$1; shift
  perl -e '
    my $s = shift @ARGV;
    my $out = "";
    eval {
      local $SIG{ALRM} = sub { die "TIMEOUT\n" };
      alarm $s;
      open(my $fh, "-|", @ARGV) or die "NOEXEC\n";
      local $/; $out = <$fh>; close $fh;
      alarm 0;
    };
    if ($@ =~ /TIMEOUT/) { print $out; print "\n__TIMEOUT__\n"; exit 124; }
    if ($@ =~ /NOEXEC/)  { print "\n__NOEXEC__\n"; exit 127; }
    print $out;
  ' "$secs" "$@" 2>&1
}

findbin() {
  local b=$1 c
  for c in "$b" "/opt/homebrew/bin/$b" "$HOME/.local/bin/$b" "$HOME/.cargo/bin/$b" "/usr/local/bin/$b" "/usr/bin/$b"; do
    if command -v "$c" >/dev/null 2>&1; then command -v "$c"; return 0; fi
    if [ -x "$c" ]; then echo "$c"; return 0; fi
  done
  return 1
}

classify() { # $1=raw output  -> prints VERDICT|DETAIL
  local o=$1
  local lo; lo=$(printf '%s' "$o" | tr 'A-Z' 'a-z')
  if printf '%s' "$o" | grep -q "PONG"; then echo "OK|live response"; return; fi
  if printf '%s' "$o" | grep -q "__TIMEOUT__"; then echo "TIMEOUT|no reply within limit"; return; fi
  if printf '%s' "$o" | grep -q "__NOEXEC__"; then echo "ERROR|could not exec"; return; fi
  case "$lo" in
    *unauthorized*|*"not logged"*|*"please login"*|*"please log in"*|*authenticate*|*"api key"*|*"no credentials"*|*"credential"*|*expired*|*401*|*403*|*"invalid token"*|*"sign in"*|*"log in"*)
      echo "AUTH|login/auth failure"; return;;
  esac
  # trim detail to one short line
  local d; d=$(printf '%s' "$o" | tr '\n' ' ' | sed 's/  */ /g' | cut -c1-120)
  echo "ERROR|${d:-no output}"
}

probe() { # $1=cli  $2=version-args  $3..=prompt cmd
  local cli=$1; shift
  local vargs=$1; shift
  local bin
  if ! bin=$(findbin "$cli"); then
    echo "$host|$cli|MISSING|-|-|not in PATH or known dirs"; return
  fi
  local ver; ver=$(run_to 12 "$bin" $vargs 2>&1 | grep -viE '__TIMEOUT__|__NOEXEC__' | head -1 | tr -d '\r' | cut -c1-40)
  [ -z "$ver" ] && ver="?"
  local raw; raw=$(run_to 90 "$@")
  local res; res=$(classify "$raw")
  echo "$host|$cli|${res%%|*}|$bin|$ver|${res#*|}"
}

# --- the three probes (bin is re-resolved inside; $CLI resolves via findbin) ---
CODEX=$(findbin codex);  CODEX=${CODEX:-codex}
CLAUDE=$(findbin claude); CLAUDE=${CLAUDE:-claude}
KIMI=$(findbin kimi);    KIMI=${KIMI:-kimi}

probe codex  "--version" "$CODEX"  exec --skip-git-repo-check "Reply with only the word: PONG"
probe claude "--version" "$CLAUDE" -p "Reply with only the word: PONG" --output-format text
probe kimi   "--version" "$KIMI"   --print --yes --prompt "Reply with only the word: PONG"
