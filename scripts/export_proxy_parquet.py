#!/usr/bin/env python3
"""Экспорт cc-proxy транспортных наблюдений (эпик proxy-correlation) в Parquet.

Читает index.db (proxy_observations ⋈ turns по request_id через LEFT JOIN) и пишет
proxy.parquet — вход для панелей analyze-usage skill (TRANSPORT RADAR / OVERLOAD
TIMELINE). Skill рендерит панели ТОЛЬКО если proxy.parquet существует (gated) —
машины без cc-proxy деградируют к обычному поведению.

Использование:
    python3 scripts/export_proxy_parquet.py [--db PATH] [--out proxy.parquet]

Требует: polars (pip3 install polars). БД создаётся командой `agent-usage index`.
"""
from __future__ import annotations

import argparse
import os
import sqlite3
import sys
from pathlib import Path


def default_db() -> Path:
    xdg = os.environ.get("XDG_CACHE_HOME")
    base = Path(xdg) if xdg else Path.home() / ".cache"
    return base / "devboy-tools-agent-usage" / "index.db"


# LEFT JOIN: транспорт + (если есть) контент турна. Никогда не INNER — orphan
# proxy-строки (ретраи без дошедшего request_id) = сигнал троттлинга, не шум.
QUERY = """
SELECT
    p.id              AS proxy_id,
    p.host            AS host,
    p.request_id      AS request_id,
    p.session_id      AS session_id,
    p.ts_ms           AS ts_ms,
    p.wait_ms         AS wait_ms,
    p.dur_ms          AS dur_ms,
    p.inflight_at_start AS inflight_at_start,
    p.status          AS status,
    p.sse_error       AS sse_error,
    p.model           AS model,
    p.cache_read      AS proxy_cache_read,
    p.cache_create    AS proxy_cache_create,
    -- контент-сторона (NULL если турн ещё не проиндексирован с request_id)
    t.id              AS turn_id,
    t.project         AS project,
    t.tokens_input    AS turn_input,
    t.tokens_output   AS turn_output,
    t.tokens_cache_read   AS turn_cache_read,
    t.tokens_cache_create AS turn_cache_create,
    t.cost_usd        AS turn_cost_usd,
    CASE WHEN t.id IS NOT NULL THEN 1 ELSE 0 END AS joined
FROM proxy_observations p
LEFT JOIN turns t
       ON t.host = p.host AND t.request_id = p.request_id
ORDER BY p.ts_ms
"""


def main() -> None:
    ap = argparse.ArgumentParser(description="Экспорт cc-proxy наблюдений в Parquet")
    ap.add_argument("--db", type=Path, default=default_db())
    ap.add_argument("--out", type=Path, default=Path("proxy.parquet"))
    a = ap.parse_args()

    if not a.db.exists():
        sys.exit(f"нет index.db: {a.db} (запусти `agent-usage index`)")

    try:
        import polars as pl
    except ImportError:
        sys.exit("нужен polars: pip3 install polars")

    conn = sqlite3.connect(str(a.db))
    # gated: нет таблицы proxy_observations (старый индекс) → пустой выход, не ошибка
    has = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='proxy_observations'"
    ).fetchone()
    if not has:
        print("proxy_observations нет (индекс < v8) — пропускаю", file=sys.stderr)
        return

    cur = conn.execute(QUERY)
    cols = [d[0] for d in cur.description]
    rows = [dict(zip(cols, r)) for r in cur.fetchall()]
    conn.close()

    if not rows:
        print("0 proxy-наблюдений — proxy.parquet не пишу (skill пропустит панели)", file=sys.stderr)
        return

    df = pl.DataFrame(rows)
    df.write_parquet(a.out, compression="zstd")
    joined = sum(r["joined"] for r in rows)
    print(
        f"✓ {a.out}: {len(rows)} наблюдений, joined={joined} "
        f"({100*joined//max(len(rows),1)}% coverage), {a.out.stat().st_size} байт"
    )


if __name__ == "__main__":
    main()
