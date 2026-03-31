"""ForgeFleet CLI entrypoint."""
from __future__ import annotations

import argparse
import json

from .engine.model_governance import ModelGovernance
from .engine.status_reporter import StatusReporter


def cmd_status(_args):
    r = StatusReporter()
    print(r.generate_report())


def cmd_recommend(args):
    gov = ModelGovernance()
    result = gov.recommend_from_history(args.task_type) or {"task_type": args.task_type, "message": "No recommendation yet"}
    print(json.dumps(result, indent=2))


def cmd_model_stats(args):
    gov = ModelGovernance()
    stats = gov.summarize_model_performance(args.task_type)
    print(json.dumps(stats, indent=2))


def main():
    parser = argparse.ArgumentParser(prog="forgefleet")
    sub = parser.add_subparsers(dest="command")

    p_status = sub.add_parser("status", help="Show fleet status")
    p_status.set_defaults(func=cmd_status)

    p_rec = sub.add_parser("recommend", help="Recommend a model for a task type from governance history")
    p_rec.add_argument("task_type")
    p_rec.set_defaults(func=cmd_recommend)

    p_stats = sub.add_parser("model-stats", help="Show model performance summary for a task type")
    p_stats.add_argument("task_type")
    p_stats.set_defaults(func=cmd_model_stats)

    args = parser.parse_args()
    if not hasattr(args, "func"):
        parser.print_help()
        return 1
    return args.func(args) or 0


if __name__ == "__main__":
    raise SystemExit(main())
