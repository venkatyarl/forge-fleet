FROM pgvector/pgvector:pg16

RUN apt-get update \
    && apt-get install -y --no-install-recommends curl gettext-base python3-pip \
    && pip3 install --break-system-packages --no-cache-dir "patroni[etcd3]==4.0.6" "psycopg[binary]>=3.2,<4" \
    && rm -rf /var/lib/apt/lists/*

COPY entrypoint.sh /usr/local/bin/forgefleet-patroni-entrypoint
RUN chmod 0755 /usr/local/bin/forgefleet-patroni-entrypoint

USER postgres
ENTRYPOINT ["/usr/local/bin/forgefleet-patroni-entrypoint"]
