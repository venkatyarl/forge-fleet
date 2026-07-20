#!/bin/sh
set -eu

: "${POSTGRES_PASSWORD:?set POSTGRES_PASSWORD}"
: "${PGCAT_ADMIN_PASSWORD:?set PGCAT_ADMIN_PASSWORD}"

# Escape sed replacement metacharacters. Quotes, backslashes, and newlines are
# rejected because they cannot be represented safely in these TOML strings.
case "$POSTGRES_PASSWORD$PGCAT_ADMIN_PASSWORD" in
  *'"'*|*'\'*|*"
"*) echo 'pgcat passwords must not contain quotes, backslashes, or newlines' >&2; exit 2 ;;
esac
postgres_password=$(printf '%s' "$POSTGRES_PASSWORD" | sed 's/[&|]/\\&/g')
admin_password=$(printf '%s' "$PGCAT_ADMIN_PASSWORD" | sed 's/[&|]/\\&/g')

sed -e "s|__POSTGRES_PASSWORD__|$postgres_password|g" \
    -e "s|__PGCAT_ADMIN_PASSWORD__|$admin_password|g" \
    /etc/pgcat/pgcat.toml.in >/tmp/pgcat.toml
exec pgcat /tmp/pgcat.toml
