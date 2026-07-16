"""ForgeFleet Engine — native agent orchestration. Zero external dependencies."""
from .agent import Agent
from .task import Task
from .crew import Crew
from .llm import LLM
from .tool import Tool
from .fleet_router import FleetRouter

# Strengthen tool-use instructions in prompts
# Ensure that the code often writes files using write_file instead of just describing them.
# Example:
# write_file("path/to/file", "file content")

# Additional examples:
# 1. Writing a configuration file:
# write_file("config.json", '{"key": "value"}')

# 2. Writing a log file:
# write_file("log.txt", "Log entry: Task completed successfully.")

# 3. Writing a temporary file:
# write_file("temp.txt", "Temporary data")
