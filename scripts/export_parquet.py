#!/usr/bin/env python3
"""
Экспорт данных devboy-agent-usage в Parquet для внутреннего анализа.

Использование:
    python3 scripts/export_parquet.py --from 2026-04-01 [--to 2026-04-30] [--out tasks.parquet]

Требует: polars (pip3 install polars)
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional


def run_export(from_date: str, to_date: Optional[str], with_aw: bool) -> list[dict]:
    cmd = ["devboy-agent-usage", "tasks", "--from", from_date, "--format", "json"]
    if to_date:
        cmd += ["--to", to_date]
    if with_aw:
        cmd.append("--with-aw")

    result = subprocess.run(cmd, capture_output=True, text=True)

    # Первая строка — человеческий заголовок ("Tasks from N sessions..."), пропускаем
    lines = result.stdout.splitlines()
    json_start = next((i for i, l in enumerate(lines) if l.strip().startswith("[")), None)
    if json_start is None:
        print("Ошибка: нет JSON в выводе", file=sys.stderr)
        print(result.stderr, file=sys.stderr)
        sys.exit(1)

    json_text = "\n".join(lines[json_start:])
    return json.loads(json_text)


def flatten_tasks(tasks: list[dict]) -> list[dict]:
    rows = []
    for t in tasks:
        tc = t.get("tool_calls", {})
        rows.append({
            # Идентификаторы
            "display_id":        t.get("display_id"),
            "task_id":           t.get("task_id"),
            "title":             t.get("title"),
            "project":           t.get("project"),
            "group_source":      t.get("group_source"),
            "status":            t.get("status"),
            # Временные метки
            "first_seen":        t.get("first_seen"),
            "last_seen":         t.get("last_seen"),
            "span_days":         _span_days(t.get("first_seen"), t.get("last_seen")),
            # Сессии и turns
            "session_count":     t.get("session_count", 0),
            "turn_count":        t.get("turn_count", 0),
            "human_turn_count":  t.get("human_turn_count", 0),
            # Время (в часах — удобнее для анализа)
            "agent_time_h":      _to_hours(t.get("agent_time_secs")),
            "human_time_h":      _to_hours(t.get("human_time_secs")),
            "dirty_human_time_h":_to_hours(t.get("dirty_human_time_secs")),
            # Производные метрики
            "human_to_agent_ratio": _ratio(t.get("human_time_secs"), t.get("agent_time_secs")),
            "dirty_to_agent_ratio": _ratio(t.get("dirty_human_time_secs"), t.get("agent_time_secs")),
            # Стоимость
            "cost_usd":          t.get("cost_usd", 0.0),
            "cost_per_agent_h":  _cost_per_h(t.get("cost_usd"), t.get("agent_time_secs")),
            "cost_per_human_h":  _cost_per_h(t.get("cost_usd"), t.get("human_time_secs")),
            # Tool calls
            "tool_total":        tc.get("total", 0),
            "tool_read":         tc.get("read", 0),
            "tool_write":        tc.get("write", 0),
            "tool_bash":         tc.get("bash", 0),
            "tool_mcp":          tc.get("mcp", 0),
            "tool_devboy":       tc.get("devboy", 0),
            # Tool call density (per agent hour)
            "tools_per_agent_h": _ratio(tc.get("total"), t.get("agent_time_secs") / 3600 if t.get("agent_time_secs") else None),
            # Session IDs (строка через запятую)
            "session_ids":       ",".join(t.get("session_ids", [])),
        })
    return rows


def _to_hours(secs) -> Optional[float]:
    if secs is None:
        return None
    return round(secs / 3600, 4)


def _ratio(numerator, denominator) -> Optional[float]:
    if numerator is None or denominator is None or denominator == 0:
        return None
    return round(numerator / denominator, 4)


def _cost_per_h(cost, secs) -> Optional[float]:
    if cost is None or secs is None or secs == 0:
        return None
    return round(cost / (secs / 3600), 2)


def _span_days(first: Optional[str], last: Optional[str]) -> Optional[float]:
    if not first or not last:
        return None
    try:
        f = datetime.fromisoformat(first.replace("Z", "+00:00"))
        l = datetime.fromisoformat(last.replace("Z", "+00:00"))
        return round((l - f).total_seconds() / 86400, 2)
    except Exception:
        return None


def main():
    parser = argparse.ArgumentParser(description="Экспорт задач в Parquet")
    parser.add_argument("--from", dest="from_date", required=True, help="Начало периода (YYYY-MM-DD)")
    parser.add_argument("--to", dest="to_date", default=None, help="Конец периода (YYYY-MM-DD)")
    parser.add_argument("--out", default="tasks.parquet", help="Путь к выходному файлу")
    parser.add_argument("--no-aw", action="store_true", help="Не использовать ActivityWatch данные")
    args = parser.parse_args()

    print(f"Загружаю задачи с {args.from_date}...", file=sys.stderr)
    tasks = run_export(args.from_date, args.to_date, with_aw=not args.no_aw)
    print(f"Получено {len(tasks)} задач", file=sys.stderr)

    rows = flatten_tasks(tasks)

    try:
        import polars as pl
    except ImportError:
        print("Установите polars: pip3 install polars", file=sys.stderr)
        sys.exit(1)

    df = pl.DataFrame(rows, infer_schema_length=len(rows))

    # Конвертируем временные метки в Datetime
    for col in ("first_seen", "last_seen"):
        df = df.with_columns(
            pl.col(col).str.to_datetime(format="%+", strict=False, ambiguous="earliest")
        )

    out_path = Path(args.out)
    df.write_parquet(out_path, compression="zstd")

    print(f"\nСохранено: {out_path} ({out_path.stat().st_size // 1024} KB)", file=sys.stderr)
    print(f"Строк: {len(df)}, колонок: {len(df.columns)}", file=sys.stderr)
    print("\nСхема:", file=sys.stderr)
    for name, dtype in zip(df.columns, df.dtypes):
        print(f"  {name:30} {dtype}", file=sys.stderr)

    # Краткая сводка прямо в stdout
    print("\n--- Сводка по задачам с AW данными ---")
    has_aw = df.filter(pl.col("human_time_h").is_not_null())
    print(f"Задач с AW данными: {len(has_aw)} / {len(df)}")
    if len(has_aw) > 0:
        print(f"\nТоп-10 по human_time_h:")
        top = (
            has_aw
            .sort("human_time_h", descending=True)
            .head(10)
            .select(["display_id", "project", "agent_time_h", "human_time_h",
                     "dirty_human_time_h", "cost_usd", "human_to_agent_ratio"])
        )
        print(top)

        print(f"\nАгрегат по проектам (сумма по project):")
        by_project = (
            has_aw
            .group_by("project")
            .agg([
                pl.sum("agent_time_h").alias("agent_h_total"),
                pl.sum("human_time_h").alias("human_h_total"),
                pl.sum("cost_usd").alias("cost_total"),
                pl.len().alias("task_count"),
            ])
            .sort("human_h_total", descending=True)
            .head(10)
        )
        print(by_project)


if __name__ == "__main__":
    main()
