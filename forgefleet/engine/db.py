"""Database helpers for ForgeFleet canonical Postgres storage."""
from __future__ import annotations

from urllib.parse import urlparse

from .. import config


def parse_database_url(url: str | None = None) -> dict:
    url = url or config.get_database_url()
    parsed = urlparse(url)
    return {
        "scheme": parsed.scheme,
        "host": parsed.hostname or "127.0.0.1",
        "port": parsed.port or 5432,
        "user": parsed.username or "forgefleet",
        "password": parsed.password or "forgefleet",
        "database": (parsed.path or "/forgefleet").lstrip("/"),
    }


def connect():
    try:
        import psycopg
    except ImportError as e:
        raise RuntimeError("psycopg is required for ForgeFleet Postgres support") from e

    db = parse_database_url()
    return psycopg.connect(
        host=db["host"],
        port=db["port"],
        user=db["user"],
        password=db["password"],
        dbname=db["database"],
        autocommit=True,
    )
