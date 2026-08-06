#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HELPER="$ROOT_DIR/scripts/forgefleet-falkor-source-firewall"
UNIT="$ROOT_DIR/deploy/systemd/forgefleet-falkordb-source-firewall.service"
TEST_DIR="$(mktemp -d)"
trap 'rm -rf "$TEST_DIR"' EXIT

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

assert_contains() {
    local haystack="$1" needle="$2"
    grep -Fq -- "$needle" <<<"$haystack" || fail "expected output to contain: $needle"
}

assert_not_contains() {
    local haystack="$1" needle="$2"
    if grep -Fq -- "$needle" <<<"$haystack"; then
        fail "expected output not to contain: $needle"
    fi
}

mkdir -p "$TEST_DIR/bin" "$TEST_DIR/state"

cat >"$TEST_DIR/bin/ip" <<'MOCK_IP'
#!/usr/bin/env bash
set -Eeuo pipefail
case "$*" in
    "link show dev enp3s0")
        [[ "${FF_FIREWALL_MOCK_NO_INTERFACE:-0}" == 0 ]] || exit 1
        printf '2: enp3s0: <BROADCAST,MULTICAST,UP> mtu 1500\n'
        ;;
    "-4 -o addr show dev enp3s0 scope global")
        printf '2: enp3s0 inet 192.168.5.104/24 brd 192.168.5.255 scope global enp3s0\n'
        ;;
    "-4 -o addr show")
        printf '1: lo inet 127.0.0.1/8 scope host lo\n'
        printf '2: enp3s0 inet 192.168.5.104/24 scope global enp3s0\n'
        if [[ "${FF_FIREWALL_MOCK_SOURCE_LOCAL:-0}" == 1 ]]; then
            printf '3: test inet 192.168.5.103/32 scope global test\n'
        fi
        ;;
    "-4 route get 192.168.5.103 from 192.168.5.104")
        if [[ "${FF_FIREWALL_MOCK_BAD_ROUTE:-0}" == 1 ]]; then
            printf '192.168.5.103 from 192.168.5.104 dev wrong0 src 192.168.5.104 uid 0\n'
        else
            printf '192.168.5.103 from 192.168.5.104 dev enp3s0 src 192.168.5.104 uid 0\n'
        fi
        ;;
    *)
        printf 'unexpected mocked ip invocation: %s\n' "$*" >&2
        exit 64
        ;;
esac
MOCK_IP

cat >"$TEST_DIR/bin/iptables" <<'MOCK_IPTABLES'
#!/usr/bin/env bash
set -Eeuo pipefail
state="${FF_FIREWALL_MOCK_STATE:?}/v4.rules"
[[ -f "$state" ]] || printf '%s\n' '-A DOCKER-USER -j RETURN' >"$state"
args=" $* "
if [[ "$args" == *" -m conntrack --help "* ]]; then
    if [[ "${FF_FIREWALL_MOCK_NO_CONNTRACK:-0}" == 1 ]]; then
        printf '%s\n' 'conntrack matcher unavailable'
    else
        printf '%s\n' '--ctdir ORIGINAL' '--ctorigdst address[/mask]' '--ctorigdstport port'
    fi
    exit 0
fi
if [[ "$args" == *" -L DOCKER-USER "* ]]; then
    [[ "${FF_FIREWALL_MOCK_NO_DOCKER_CHAIN:-0}" == 0 ]] || exit 1
    exit 0
fi
if [[ "$args" == *" -S DOCKER-USER "* ]]; then
    [[ "${FF_FIREWALL_MOCK_NO_DOCKER_CHAIN:-0}" == 0 ]] || exit 1
    if [[ "${FF_FIREWALL_MOCK_NO_DOCKER_RETURN:-0}" == 1 ]]; then
        grep -v -- '-j RETURN' "$state" || true
    else
        cat "$state"
    fi
    exit 0
fi
if [[ "$args" == *" -C DOCKER-USER "* ]]; then
    if [[ "$args" == *" forgefleet:falkor:allow-v4:v1 "* ]]; then
        grep -F 'forgefleet:falkor:allow-v4:v1' "$state" |
            grep -F -- '-s 192.168.5.103/32' | grep -F -- '-j ACCEPT' >/dev/null
        exit
    fi
    if [[ "$args" == *" forgefleet:falkor:deny-v4:v1 "* ]]; then
        grep -F 'forgefleet:falkor:deny-v4:v1' "$state" |
            grep -F -- '--ctorigdst 192.168.5.104/32' | grep -F -- '-j DROP' >/dev/null
        exit
    fi
fi
if [[ "$args" == *" -I DOCKER-USER "* ]]; then
    if [[ "$args" == *" forgefleet:falkor:allow-v4:v1 "* ]]; then
        line='-A DOCKER-USER -i enp3s0 -p tcp -s 192.168.5.103/32 -m conntrack --ctdir ORIGINAL --ctorigdst 192.168.5.104/32 --ctorigdstport 63379 -m comment --comment "forgefleet:falkor:allow-v4:v1" -j ACCEPT'
    elif [[ "$args" == *" forgefleet:falkor:deny-v4:v1 "* ]]; then
        line='-A DOCKER-USER -i enp3s0 -p tcp -m conntrack --ctdir ORIGINAL --ctorigdst 192.168.5.104/32 --ctorigdstport 63379 -m comment --comment "forgefleet:falkor:deny-v4:v1" -j DROP'
    else
        exit 66
    fi
    temporary="$(mktemp)"
    { printf '%s\n' "$line"; cat "$state"; } >"$temporary"
    mv "$temporary" "$state"
    chmod 0644 "$state"
    printf 'v4 insert %s\n' "$line" >>"${FF_FIREWALL_MOCK_LOG:?}"
    exit 0
fi
if [[ "$args" == *" -D DOCKER-USER "* ]]; then
    if [[ "$args" == *" forgefleet:falkor:allow-v4:v1 "* ]]; then
        tag='forgefleet:falkor:allow-v4:v1'
    elif [[ "$args" == *" forgefleet:falkor:deny-v4:v1 "* ]]; then
        tag='forgefleet:falkor:deny-v4:v1'
    else
        exit 66
    fi
    temporary="$(mktemp)"
    grep -Fv "$tag" "$state" >"$temporary" || true
    mv "$temporary" "$state"
    chmod 0644 "$state"
    printf 'v4 delete %s\n' "$tag" >>"${FF_FIREWALL_MOCK_LOG:?}"
    exit 0
fi
printf 'unexpected mutating mocked iptables invocation: %s\n' "$*" >&2
exit 65
MOCK_IPTABLES

cat >"$TEST_DIR/bin/ip6tables" <<'MOCK_IP6TABLES'
#!/usr/bin/env bash
set -Eeuo pipefail
state="${FF_FIREWALL_MOCK_STATE:?}/v6.rules"
touch "$state"
args=" $* "
if [[ "$args" == *" -m conntrack --help "* ]]; then
    printf '%s\n' '--ctdir ORIGINAL' '--ctorigdstport port'
    exit 0
fi
if [[ "$args" == *" -L INPUT "* ]]; then
    exit 0
fi
if [[ "$args" == *" -L FORWARD "* ]]; then
    exit 0
fi
if [[ "$args" == *" -S INPUT "* ]]; then
    grep -E '^-A INPUT ' "$state" || true
    exit 0
fi
if [[ "$args" == *" -S FORWARD "* ]]; then
    grep -E '^-A FORWARD ' "$state" || true
    exit 0
fi
if [[ "$args" == *" -C INPUT "* ]]; then
    grep -F 'forgefleet:falkor:deny-v6:v1' "$state" |
        grep -F -- '--dport 63379' | grep -F -- '-j DROP' >/dev/null
    exit
fi
if [[ "$args" == *" -C FORWARD "* ]]; then
    grep -F 'forgefleet:falkor:deny-v6-forward:v1' "$state" |
        grep -F -- '--ctorigdstport 63379' | grep -F -- '-j DROP' >/dev/null
    exit
fi
if [[ "$args" == *" -I INPUT "* ]]; then
    line='-A INPUT -i enp3s0 -p tcp --dport 63379 -m comment --comment "forgefleet:falkor:deny-v6:v1" -j DROP'
    printf '%s\n' "$line" >>"$state"
    printf 'v6 insert %s\n' "$line" >>"${FF_FIREWALL_MOCK_LOG:?}"
    exit 0
fi
if [[ "$args" == *" -I FORWARD "* ]]; then
    line='-A FORWARD -i enp3s0 -p tcp -m conntrack --ctdir ORIGINAL --ctorigdstport 63379 -m comment --comment "forgefleet:falkor:deny-v6-forward:v1" -j DROP'
    printf '%s\n' "$line" >>"$state"
    printf 'v6 insert %s\n' "$line" >>"${FF_FIREWALL_MOCK_LOG:?}"
    exit 0
fi
if [[ "$args" == *" -D INPUT "* || "$args" == *" -D FORWARD "* ]]; then
    if [[ "$args" == *" forgefleet:falkor:deny-v6-forward:v1 "* ]]; then
        tag='forgefleet:falkor:deny-v6-forward:v1'
    else
        tag='forgefleet:falkor:deny-v6:v1'
    fi
    temporary="$(mktemp)"
    grep -Fv "$tag" "$state" >"$temporary" || true
    mv "$temporary" "$state"
    chmod 0644 "$state"
    printf 'v6 delete %s\n' "$tag" >>"${FF_FIREWALL_MOCK_LOG:?}"
    exit 0
fi
printf 'unexpected mutating mocked ip6tables invocation: %s\n' "$*" >&2
exit 65
MOCK_IP6TABLES

chmod +x "$TEST_DIR/bin/ip" "$TEST_DIR/bin/iptables" "$TEST_DIR/bin/ip6tables"

run_helper() {
    env \
        PATH="$TEST_DIR/bin:/usr/bin:/bin" \
        FF_FIREWALL_MOCK_STATE="$TEST_DIR/state" \
        FF_FIREWALL_MOCK_LOG="$TEST_DIR/mutations.log" \
        FF_FALKOR_INTERFACE=enp3s0 \
        FF_FALKOR_ALLOWED_SOURCE_IPV4=192.168.5.103 \
        FF_FALKOR_DESTINATION_IPV4=192.168.5.104 \
        FF_FALKOR_PORT=63379 \
        "$HELPER" "$@"
}

run_helper_with_flag() {
    local flag="$1"
    shift
    env \
        PATH="$TEST_DIR/bin:/usr/bin:/bin" \
        FF_FIREWALL_MOCK_STATE="$TEST_DIR/state" \
        FF_FIREWALL_MOCK_LOG="$TEST_DIR/mutations.log" \
        "$flag=1" \
        FF_FALKOR_INTERFACE=enp3s0 \
        FF_FALKOR_ALLOWED_SOURCE_IPV4=192.168.5.103 \
        FF_FALKOR_DESTINATION_IPV4=192.168.5.104 \
        FF_FALKOR_PORT=63379 \
        "$HELPER" "$@"
}

run_helper_root() {
    sudo -n env \
        PATH="$TEST_DIR/bin:/usr/bin:/bin" \
        FF_FIREWALL_MOCK_STATE="$TEST_DIR/state" \
        FF_FIREWALL_MOCK_LOG="$TEST_DIR/mutations.log" \
        FF_FALKOR_INTERFACE=enp3s0 \
        FF_FALKOR_ALLOWED_SOURCE_IPV4=192.168.5.103 \
        FF_FALKOR_DESTINATION_IPV4=192.168.5.104 \
        FF_FALKOR_PORT=63379 \
        "$HELPER" "$@"
}

bash -n "$HELPER"
bash -n "$0"

rendered="$(run_helper render)"
assert_contains "$rendered" 'forgefleet:falkor:allow-v4:v1'
assert_contains "$rendered" 'forgefleet:falkor:deny-v4:v1'
assert_contains "$rendered" 'forgefleet:falkor:deny-v6:v1'
assert_contains "$rendered" 'forgefleet:falkor:deny-v6-forward:v1'
assert_contains "$rendered" '--ctorigdst'
assert_contains "$rendered" '--ctorigdstport'
assert_contains "$rendered" '192.168.5.103/32'
assert_contains "$rendered" '192.168.5.104/32'

dry_apply="$(run_helper --dry-run apply)"
assert_contains "$dry_apply" 'ip6tables'
assert_contains "$dry_apply" '-I INPUT 1'
assert_contains "$dry_apply" '-I FORWARD 1'
assert_contains "$dry_apply" '-I DOCKER-USER 1'
assert_contains "$dry_apply" 'dry-run apply complete'
assert_not_contains "$dry_apply" '55432'
assert_not_contains "$dry_apply" '56379'
deny_line="$(grep -n 'deny-v4' <<<"$dry_apply" | cut -d: -f1)"
allow_line="$(grep -n 'allow-v4' <<<"$dry_apply" | cut -d: -f1)"
((deny_line < allow_line)) || fail "deny must be installed before position-1 allow"

for missing_prerequisite in \
    FF_FIREWALL_MOCK_NO_INTERFACE \
    FF_FIREWALL_MOCK_SOURCE_LOCAL \
    FF_FIREWALL_MOCK_BAD_ROUTE \
    FF_FIREWALL_MOCK_NO_DOCKER_CHAIN \
    FF_FIREWALL_MOCK_NO_DOCKER_RETURN \
    FF_FIREWALL_MOCK_NO_CONNTRACK; do
    if run_helper_with_flag "$missing_prerequisite" --dry-run apply >/dev/null 2>&1; then
        fail "apply did not fail closed for $missing_prerequisite"
    fi
done

if empty_json="$(run_helper --dry-run --json status 2>/dev/null)"; then
    fail "empty mocked firewall unexpectedly reported applied"
fi
assert_contains "$empty_json" '"ok":false'

cat >"$TEST_DIR/state/v4.rules" <<'RULES_V4'
-A DOCKER-USER -i enp3s0 -p tcp -s 192.168.5.103/32 -m conntrack --ctdir ORIGINAL --ctorigdst 192.168.5.104/32 --ctorigdstport 63379 -m comment --comment "forgefleet:falkor:allow-v4:v1" -j ACCEPT
-A DOCKER-USER -i enp3s0 -p tcp -m conntrack --ctdir ORIGINAL --ctorigdst 192.168.5.104/32 --ctorigdstport 63379 -m comment --comment "forgefleet:falkor:deny-v4:v1" -j DROP
-A DOCKER-USER -j RETURN
-A DOCKER-USER -p tcp --dport 55432 -j ACCEPT
-A DOCKER-USER -p tcp --dport 56379 -j ACCEPT
RULES_V4
cat >"$TEST_DIR/state/v6.rules" <<'RULES_V6'
-A INPUT -i enp3s0 -p tcp --dport 63379 -m comment --comment "forgefleet:falkor:deny-v6:v1" -j DROP
-A FORWARD -i enp3s0 -p tcp -m conntrack --ctdir ORIGINAL --ctorigdstport 63379 -m comment --comment "forgefleet:falkor:deny-v6-forward:v1" -j DROP
RULES_V6

status="$(run_helper --dry-run status)"
assert_contains "$status" 'status=applied'
assert_contains "$status" 'ipv4_allow=1 position=1'
assert_contains "$status" 'ipv4_deny=1 position=2'
assert_contains "$status" 'ipv6_deny=1 position=1'
assert_contains "$status" 'ipv6_forward_deny=1 position=1'

json_status="$(run_helper --dry-run --json status)"
assert_contains "$json_status" '"ok":true'
assert_contains "$json_status" '"interface":"enp3s0"'
assert_contains "$json_status" '"source_ipv4":"192.168.5.103"'
assert_contains "$json_status" '"destination_ipv4":"192.168.5.104"'
assert_contains "$json_status" '"port":63379'
assert_contains "$json_status" '"allow_v4":true'
assert_contains "$json_status" '"deny_v4":true'
assert_contains "$json_status" '"deny_v6":true'
assert_contains "$json_status" '"unit":"forgefleet-falkordb-source-firewall.service"'
python3 -c 'import json,sys; json.load(sys.stdin)' <<<"$json_status"

# A restart after only the IPv6 rule was lost must safely repair that partial
# state without disturbing the already-correct IPv4 allow/deny pair.
: >"$TEST_DIR/state/v6.rules"
partial_apply="$(run_helper --dry-run apply)"
assert_contains "$partial_apply" 'ip6tables'
assert_not_contains "$partial_apply" 'DOCKER-USER'
cat >"$TEST_DIR/state/v6.rules" <<'RULES_V6'
-A INPUT -i enp3s0 -p tcp --dport 63379 -m comment --comment "forgefleet:falkor:deny-v6:v1" -j DROP
-A FORWARD -i enp3s0 -p tcp -m conntrack --ctdir ORIGINAL --ctorigdstport 63379 -m comment --comment "forgefleet:falkor:deny-v6-forward:v1" -j DROP
RULES_V6

dry_remove="$(run_helper --dry-run remove)"
assert_contains "$dry_remove" '-D DOCKER-USER'
assert_contains "$dry_remove" '-D INPUT'
assert_contains "$dry_remove" '-D FORWARD'
assert_not_contains "$dry_remove" ' -F '
assert_not_contains "$dry_remove" '55432'
assert_not_contains "$dry_remove" '56379'
if grep -Eq -- '(^|[[:space:]])(-F|--flush)([[:space:]]|$)' "$HELPER"; then
    fail "helper must never flush a firewall chain"
fi

# Removal is rollback, so it must use the exact discovered tagged rules even
# after policy values drift, and it must not depend on the audited route/link.
drift_remove="$(env \
    PATH="$TEST_DIR/bin:/usr/bin:/bin" \
    FF_FIREWALL_MOCK_STATE="$TEST_DIR/state" \
    FF_FALKOR_INTERFACE=changed0 \
    FF_FALKOR_ALLOWED_SOURCE_IPV4=192.0.2.10 \
    FF_FALKOR_DESTINATION_IPV4=192.0.2.20 \
    FF_FALKOR_PORT=63380 \
    "$HELPER" --dry-run remove)"
assert_contains "$drift_remove" '192.168.5.103/32'
assert_contains "$drift_remove" '192.168.5.104/32'
assert_contains "$drift_remove" '63379'
run_helper_with_flag FF_FIREWALL_MOCK_NO_INTERFACE --dry-run remove >/dev/null
run_helper_with_flag FF_FIREWALL_MOCK_BAD_ROUTE --dry-run remove >/dev/null
run_helper_with_flag FF_FIREWALL_MOCK_NO_DOCKER_CHAIN --dry-run remove >/dev/null
env PATH="$TEST_DIR/bin:/usr/bin:/bin" \
    FF_FIREWALL_MOCK_STATE="$TEST_DIR/state" \
    FF_FIREWALL_MOCK_LOG="$TEST_DIR/mutations.log" \
    "$HELPER" --dry-run remove >/dev/null

saved_v4="$(<"$TEST_DIR/state/v4.rules")"
cat >"$TEST_DIR/state/v4.rules" <<'FOREIGN_TAG'
-A DOCKER-USER -p tcp --dport 1 -m comment --comment "forgefleet:falkor:allow-v4:v1" -j ACCEPT
-A DOCKER-USER -j RETURN
FOREIGN_TAG
if run_helper --dry-run remove >/dev/null 2>&1; then
    fail "remove accepted a reserved tag attached to a foreign rule shape"
fi
printf '%s\n' "$saved_v4" >"$TEST_DIR/state/v4.rules"

# Exercise real (mocked-front-end) mutations as root: converge from Docker's
# baseline, prove idempotent second apply, then remove only owned tags while
# preserving Docker RETURN and unrelated PostgreSQL/Redis rules.
cat >"$TEST_DIR/state/v4.rules" <<'BASELINE_V4'
-A DOCKER-USER -j RETURN
-A DOCKER-USER -p tcp --dport 55432 -j ACCEPT
-A DOCKER-USER -p tcp --dport 56379 -j ACCEPT
BASELINE_V4
: >"$TEST_DIR/state/v6.rules"
: >"$TEST_DIR/mutations.log"
run_helper_root apply >/dev/null
root_status="$(run_helper_root --json status)"
assert_contains "$root_status" '"ok":true'
mutations_after_first_apply="$(wc -l <"$TEST_DIR/mutations.log")"
run_helper_root apply >/dev/null
[[ "$(wc -l <"$TEST_DIR/mutations.log")" == "$mutations_after_first_apply" ]] ||
    fail "idempotent apply emitted additional mutations"
run_helper_root remove >/dev/null
assert_not_contains "$(<"$TEST_DIR/state/v4.rules")" 'forgefleet:falkor:'
assert_not_contains "$(<"$TEST_DIR/state/v6.rules")" 'forgefleet:falkor:'
assert_contains "$(<"$TEST_DIR/state/v4.rules")" '-j RETURN'
assert_contains "$(<"$TEST_DIR/state/v4.rules")" '--dport 55432'
assert_contains "$(<"$TEST_DIR/state/v4.rules")" '--dport 56379'

if env \
    FF_FALKOR_INTERFACE=enp3s0 \
    FF_FALKOR_ALLOWED_SOURCE_IPV4=192.168.5.103 \
    FF_FALKOR_DESTINATION_IPV4=192.168.5.104 \
    FF_FALKOR_PORT=55432 \
    "$HELPER" render >/dev/null 2>&1; then
    fail "reserved PostgreSQL port was accepted"
fi

grep -Fq 'Requires=docker.service' "$UNIT" || fail "unit must require docker.service"
grep -Fq 'After=network-online.target docker.service' "$UNIT" || fail "unit ordering missing"
grep -Fq 'PartOf=docker.service' "$UNIT" || fail "unit must stop with docker.service"
grep -Fq 'WantedBy=multi-user.target docker.service' "$UNIT" ||
    fail "enabled unit must start whenever docker.service starts"
grep -Fq 'RemainAfterExit=yes' "$UNIT" || fail "unit must retain oneshot state"
grep -Fq 'EnvironmentFile=/etc/forgefleet/falkordb-source-firewall.env' "$UNIT" ||
    fail "unit EnvironmentFile missing"
grep -Fq 'ExecStart=/usr/local/sbin/forgefleet-falkor-source-firewall apply' "$UNIT" ||
    fail "unit apply action missing"
grep -Fq 'ExecStop=/usr/local/sbin/forgefleet-falkor-source-firewall remove' "$UNIT" ||
    fail "unit remove action missing"

printf 'PASS: ForgeFleet Falkor source firewall artifact tests\n'
