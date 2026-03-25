"""ForgeFleet CLI — run the distributed coding agent."""
import argparse
import json
import os
import sys
import time
from pathlib import Path

from .orchestrator.fleet_discovery import FleetDiscovery
from .orchestrator.pipeline import TieredPipeline
from .routing.task_router import TaskRouter
from .memory.store import MemoryStore
from .context.repo_context import RepoContext


def main():
    parser = argparse.ArgumentParser(description="ForgeFleet — Distributed AI Coding Agent")
    subparsers = parser.add_subparsers(dest="command")
    
    # forgefleet discover — show fleet status
    discover_parser = subparsers.add_parser("discover", help="Discover fleet nodes and models")
    
    # forgefleet run "task" — run a single task through pipeline
    run_parser = subparsers.add_parser("run", help="Run a task through the tiered pipeline")
    run_parser.add_argument("task", help="Task description")
    run_parser.add_argument("--repo", default=".", help="Repository directory")
    run_parser.add_argument("--branch", default="", help="Git branch name")
    run_parser.add_argument("--tier", type=int, default=1, help="Start at tier N")
    
    # forgefleet agent — run as autonomous agent (poll MC for tasks)
    agent_parser = subparsers.add_parser("agent", help="Run as autonomous agent")
    agent_parser.add_argument("--repo", required=True, help="Repository directory")
    agent_parser.add_argument("--mc", default="http://192.168.5.100:60002", help="MC API URL")
    agent_parser.add_argument("--node", default="", help="Node name")
    agent_parser.add_argument("--poll-interval", type=int, default=15, help="Seconds between polls")
    
    # forgefleet memory — show memory stats
    memory_parser = subparsers.add_parser("memory", help="Show shared memory stats")
    
    args = parser.parse_args()
    
    fleet = FleetDiscovery()
    memory = MemoryStore()
    
    if args.command == "discover":
        print("🔍 Discovering fleet...")
        status = fleet.discover_all()
        for name, info in status.items():
            icon = "✅" if info["connected"] else "❌"
            print(f"  {icon} {name}: {info['healthy']}/{info['total']} models healthy")
            for m in info["models"]:
                micon = "🟢" if m["healthy"] else "🔴"
                print(f"    {micon} {m['name']} (tier {m['tier']}) — {m['url']}")
    
    elif args.command == "run":
        print(f"🏗️ Running task: {args.task[:60]}")
        branch = args.branch or f"feat/forgefleet-{int(time.time())}"
        pipeline = TieredPipeline(fleet, os.path.abspath(args.repo))
        result = pipeline.run(
            task_title=args.task,
            task_description=args.task,
            branch_name=branch,
            start_tier=args.tier,
        )
        if result["success"]:
            print(f"✅ Completed at tier {result['completed_tier']}")
            print(f"   Branch: {result['branch']}")
            for c in result["commits"]:
                print(f"   Commit: {c['message']}")
        else:
            print(f"❌ Failed after tier {result['completed_tier']}")
            print(f"   {result['result'][:200]}")
    
    elif args.command == "agent":
        print(f"🤖 ForgeFleet Agent starting on {args.node or 'auto'}")
        print(f"   Repo: {args.repo}")
        print(f"   MC: {args.mc}")
        print(f"   Poll interval: {args.poll_interval}s")
        
        router = TaskRouter(fleet, memory, os.path.abspath(args.repo), args.mc, args.node)
        
        while True:
            try:
                results = router.poll_and_execute(max_tasks=1)
                if results:
                    for r in results:
                        icon = "✅" if r["success"] else "❌"
                        print(f"  {icon} Tier {r['completed_tier']}: {r['result'][:80]}")
                else:
                    pass  # No tasks available
            except KeyboardInterrupt:
                print("\n⛔ Shutting down")
                break
            except Exception as e:
                print(f"  ⚠️ Error: {e}")
            
            time.sleep(args.poll_interval)
    
    elif args.command == "memory":
        stats = memory.stats()
        print("🧠 Shared Memory Stats:")
        for k, v in stats.items():
            print(f"  {k}: {v}")
        
        errors = memory.get_error_patterns(5)
        if errors:
            print("\n  Common errors:")
            for e in errors:
                print(f"    {e['count']}x: {e['error'][:60]}")
    
    else:
        parser.print_help()


if __name__ == "__main__":
    main()
