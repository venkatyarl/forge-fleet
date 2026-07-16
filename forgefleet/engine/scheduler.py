"""Autonomous Scheduler — work when Venkat sleeps.

Detects user activity, manages resource allocation, and
runs autonomous work based on priority queues.

Activity Detection:
- macOS: ioreg idle time (keyboard/mouse)
- Linux: xprintidle or /proc/stat
- Fallback: check if SSH session is active

Resource States:
- ACTIVE: user is typing → reserve machine, don't use for fleet
- IDLE_SHORT (5min): user paused → machine joins fleet pool
- IDLE_LONG (30min): user away → start autonomous work
- NIGHT (11pm-8am): full autonomous mode
- AWAY: no activity for 2h+ → maximum autonomous work

Autonomous Work Priorities:
1. Build backlog (MC tickets)
2. Code review (pending review tickets)
3. Research (competitors, trends, patents)
4. Content (blog posts, social media)
5. Revenue (trading signals, gig matching)
"""
import json
import os
import subprocess
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from enum import Enum
from typing import Optional, Callable


class ActivityState(Enum):
    ACTIVE = "active"           # User typing right now
    IDLE_SHORT = "idle_short"   # 5+ min idle — machine joins fleet
    IDLE_LONG = "idle_long"     # 30+ min idle — start autonomous work
    NIGHT = "night"             # 11pm-8am — full autonomous mode
    AWAY = "away"               # 2h+ idle — maximum work


class WorkPriority(Enum):
    BUILD = 1       # MC tickets, coding crews
    REVIEW = 2      # Code review, QA
    RESEARCH = 3    # Competitors, trends, patents
    CONTENT = 4     # Blog, social media, marketing
    REVENUE = 5     # Trading, gigs, proposals


@dataclass
class WorkItem:
    """A unit of autonomous work."""
    priority: WorkPriority
    type: str           # "mc_ticket", "code_review", "research", "blog_post", etc.
    description: str
    project: str = ""   # Which project this belongs to
    estimated_minutes: int = 15
    requires_paid_llm: bool = False
    func: Callable = None  # Function to execute
    

@dataclass
class ProjectConfig:
    """Per-project configuration for model routing."""
    name: str
    repo_path: str
    allow_paid_llm: bool = False
    preferred_tier: int = 2  # Default to 32B for code
    max_tier: int = 3        # Don't escalate past 72B by default
    mc_project_id: str = ""  # Mission Control project filter


@dataclass
class SchedulerConfig:
    """Configuration for the autonomous scheduler."""
    idle_short_seconds: int = 300      # 5 min
    idle_long_seconds: int = 1800      # 30 min
    away_seconds: int = 7200           # 2 hours
    night_start_hour: int = 23         # 11 PM
    night_end_hour: int = 8            # 8 AM
    max_concurrent_tasks: int = 3
    poll_interval_seconds: int = 60    # Check activity every minute
    reclaim_grace_seconds: int = 30    # Time to finish task when user returns
    timezone: str = "America/New_York"


@dataclass
class AutoScheduler:
    """Autonomous work scheduler based on user activity.
    
    Core loop:
    1. Check user activity (keyboard/mouse idle time)
    2. Determine activity state (ACTIVE → IDLE → NIGHT → AWAY)
    3. Based on state, decide what work to do
    4. When user returns, gracefully yield resources
    """
    config: SchedulerConfig = field(default_factory=SchedulerConfig)
    projects: dict = field(default_factory=dict)  # name -> ProjectConfig
    work_queue: list = field(default_factory=list)  # WorkItem list, sorted by priority
    current_state: ActivityState = ActivityState.ACTIVE
    running_tasks: list = field(default_factory=list)
    callbacks: list = field(default_factory=list)
    _running: bool = False
    
    def register_project(self, project: ProjectConfig):
        """Register a project with its model routing config."""
        self.projects[project.name] = project
    
    def add_work(self, item: WorkItem):
        """Add a work item to the queue."""
        self.work_queue.append(item)
        self.work_queue.sort(key=lambda w: w.priority.value)
    
    def on_state_change(self, callback: Callable):
        """Register callback for state changes."""
        self.callbacks.append(callback)
    
    def _emit_state_change(self, old_state: ActivityState, new_state: ActivityState):
        """Notify all callbacks of state change."""
        for cb in self.callbacks:
            try:
                cb(old_state, new_state)
            except Exception:
                pass
    
    # ─── Activity Detection ─────────────────────────
    
    def get_idle_seconds(self) -> int:
        """Get seconds since last user input (keyboard/mouse).
        
        macOS: ioreg reports HIDIdleTime in nanoseconds
        Linux: xprintidle reports milliseconds
        Fallback: check if any SSH/TTY sessions are active
        """
        import platform
        
        if platform.system() == "Darwin":
            return self._macos_idle()
        elif platform.system() == "Linux":
            return self._linux_idle()
        return 0  # Unknown OS — assume active
    
    def _macos_idle(self) -> int:
        """Get idle time on macOS via IOKit."""
        try:
            r = subprocess.run(
                ["ioreg", "-c", "IOHIDSystem"],
                capture_output=True, text=True, timeout=5
            )
            for line in r.stdout.split("\n"):
                if "HIDIdleTime" in line and "=" in line:
                    # Value is in nanoseconds
                    value = line.split("=")[-1].strip()
                    ns = int(value)
                    return ns // 1_000_000_000  # Convert to seconds
        except Exception:
            pass
        return 0
    
    def _linux_idle(self) -> int:
        """Get idle time on Linux via xprintidle or /proc."""
        # Try xprintidle first (X11)
        try:
            r = subprocess.run(
                ["xprintidle"], capture_output=True, text=True, timeout=3
            )
            if r.returncode == 0:
                return int(r.stdout.strip()) // 1000  # ms to seconds
        except Exception:
            pass
        
        # Headless Linux — check TTY activity
        try:
            r = subprocess.run(
                ["who", "-u"], capture_output=True, text=True, timeout=3
            )
            # If no active TTY sessions, consider idle
            if not r.stdout.strip():
                return 99999  # Very idle
        except Exception:
            pass
        
        return 0
    
    def determine_state(self) -> ActivityState:
        """Determine the current activity state."""
        idle = self.get_idle_seconds()
        hour = datetime.now().hour
        
        # Night mode override
        if hour >= self.config.night_start_hour or hour < self.config.night_end_hour:
            if idle > self.config.idle_short_seconds:
                return ActivityState.NIGHT
        
        # Activity-based states
        if idle < self.config.idle_short_seconds:
            return ActivityState.ACTIVE
        elif idle < self.config.idle_long_seconds:
            return ActivityState.IDLE_SHORT
        elif idle < self.config.away_seconds:
            return ActivityState.IDLE_LONG
        else:
            return ActivityState.AWAY
    
    # ─── Work Management ────────────────────────────
    
    def get_allowed_work(self, state: ActivityState) -> list[WorkItem]:
        """Get work items allowed for the current state."""
        if state == ActivityState.ACTIVE:
            return []  # Don't start anything while user is active
        
        allowed_priorities = {
            ActivityState.IDLE_SHORT: {WorkPriority.BUILD, WorkPriority.REVIEW},
            ActivityState.IDLE_LONG: {WorkPriority.BUILD, WorkPriority.REVIEW, WorkPriority.RESEARCH},
            ActivityState.NIGHT: {WorkPriority.BUILD, WorkPriority.REVIEW, WorkPriority.RESEARCH, 
                                  WorkPriority.CONTENT, WorkPriority.REVENUE},
            ActivityState.AWAY: {WorkPriority.BUILD, WorkPriority.REVIEW, WorkPriority.RESEARCH,
                                 WorkPriority.CONTENT, WorkPriority.REVENUE},
        }
        
        allowed = allowed_priorities.get(state, set())
        return [w for w in self.work_queue if w.priority in allowed]
    
    def should_reclaim(self) -> bool:
        """Check if user just became active — need to yield resources."""
        current_idle = self.get_idle_seconds()
        return current_idle < 10  # User active in last 10 seconds
    
    # ─── Main Loop ──────────────────────────────────
    
    def run(self):
        """Main scheduler loop."""
        self._running = True
        print(f"🤖 Autonomous Scheduler starting")
        print(f"   Idle thresholds: short={self.config.idle_short_seconds}s, "
              f"long={self.config.idle_long_seconds}s, away={self.config.away_seconds}s")
        print(f"   Night mode: {self.config.night_start_hour}:00 - {self.config.night_end_hour}:00")
        
        while self._running:
            try:
                new_state = self.determine_state()
                
                if new_state != self.current_state:
                    old = self.current_state
                    self.current_state = new_state
                    self._emit_state_change(old, new_state)
                    
                    icons = {
                        ActivityState.ACTIVE: "👤",
                        ActivityState.IDLE_SHORT: "💤",
                        ActivityState.IDLE_LONG: "😴",
                        ActivityState.NIGHT: "🌙",
                        ActivityState.AWAY: "🏖️",
                    }
                    print(f"  {icons.get(new_state, '?')} State: {old.value} → {new_state.value}")
                
                # Check if user returned — reclaim resources
                if new_state == ActivityState.ACTIVE and self.running_tasks:
                    print(f"  👤 User active — yielding {len(self.running_tasks)} tasks")
                    self._graceful_yield()
                    continue
                
                # Get allowed work for current state
                available_work = self.get_allowed_work(new_state)
                
                if available_work and len(self.running_tasks) < self.config.max_concurrent_tasks:
                    next_item = available_work[0]
                    self.work_queue.remove(next_item)
                    self._start_work(next_item)
                
            except KeyboardInterrupt:
                self._running = False
                break
            except Exception as e:
                print(f"  ⚠️ Scheduler error: {e}")
            
            time.sleep(self.config.poll_interval_seconds)
        
        print("⛔ Scheduler stopped")
    
    def stop(self):
        """Stop the scheduler gracefully."""
        self._running = False
        self._graceful_yield()
    
    def _start_work(self, item: WorkItem):
        """Start a work item."""
        print(f"  🏗️ Starting [{item.priority.name}] {item.type}: {item.description[:60]}")
        self.running_tasks.append(item)
        
        if item.func:
            try:
                item.func()
            except Exception as e:
                print(f"  ❌ Work failed: {e}")
            finally:
                if item in self.running_tasks:
                    self.running_tasks.remove(item)
    
    def _graceful_yield(self):
        """Gracefully stop running tasks when user returns."""
        # Give tasks a grace period to finish
        if self.running_tasks:
            print(f"  ⏳ Waiting {self.config.reclaim_grace_seconds}s for tasks to finish...")
            time.sleep(self.config.reclaim_grace_seconds)
            # Force stop any remaining
            self.running_tasks.clear()
    
    # ─── Status ─────────────────────────────────────
    
    def status(self) -> dict:
        """Get scheduler status."""
        idle = self.get_idle_seconds()
        return {
            "state": self.current_state.value,
            "idle_seconds": idle,
            "idle_human": f"{idle//60}m {idle%60}s" if idle > 60 else f"{idle}s",
            "running_tasks": len(self.running_tasks),
            "queue_size": len(self.work_queue),
            "queue_by_priority": {
                p.name: len([w for w in self.work_queue if w.priority == p])
                for p in WorkPriority
            },
            "projects": list(self.projects.keys()),
            "config": {
                "idle_short": self.config.idle_short_seconds,
                "idle_long": self.config.idle_long_seconds,
                "night_hours": f"{self.config.night_start_hour}-{self.config.night_end_hour}",
                "max_concurrent": self.config.max_concurrent_tasks,
            },
        }
