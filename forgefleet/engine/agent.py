"""Agent — a role-based AI agent with tools and an LLM.

Core pattern from CrewAI's Agent + StepExecutor, simplified:
1. Agent has a role, goal, backstory, tools, and LLM
2. When given a task, it builds a prompt and calls the LLM
3. If LLM returns tool calls, execute them and feed results back
4. Loop until LLM gives a final answer (no more tool calls)
5. Return the result
"""
import json
from dataclasses import dataclass, field
from .llm import LLM
from .tool import Tool


@dataclass
class Agent:
    """An AI agent with a defined role, tools, and LLM.
    
    The agent loop (extracted from CrewAI's StepExecutor pattern):
    1. Build system prompt from role/goal/backstory
    2. Add task description + context from previous agents
    3. Call LLM (with tool schemas if tools available)
    4. If tool_calls in response -> execute tools -> add results -> call LLM again
    5. If text response -> return as final answer
    6. Max iterations prevents infinite loops
    
    Falls back to non-tool-calling mode if LLM doesn't support function calling.
    """
    role: str
    goal: str
    backstory: str = ""
    tools: list = field(default_factory=list)
    llm: LLM = field(default_factory=LLM)
    max_iterations: int = 15
    verbose: bool = False
    
    def execute(self, task_description: str, context: str = "") -> str:
        """Execute a task and return the result."""
        system_msg = self._build_system_prompt()
        
        user_content = f"## Task\n{task_description}"
        if context:
            user_content = f"## Context from previous agents\n{context}\n\n{user_content}"
        
        messages = [
            {"role": "system", "content": system_msg},
            {"role": "user", "content": user_content},
        ]
        
        # Try tool-calling mode first, fall back to manual mode
        if self.tools:
            result = self._execute_with_tools(messages)
            if result is not None:
                return result
            # Tool calling failed — fall back to manual mode
            if self.verbose:
                print(f"  [{self.role}] Tool calling not supported, using manual mode")
            return self._execute_manual(messages)
        
        # No tools — simple LLM call
        return self._execute_simple(messages)
    
    def _execute_with_tools(self, messages: list[dict]) -> str | None:
        """Execute using OpenAI function calling. Returns None if not supported."""
        tool_schemas = [t.to_openai_schema() for t in self.tools]
        tool_map = {t.name: t for t in self.tools}
        
        msgs = list(messages)  # Copy to avoid mutation
        
        for iteration in range(self.max_iterations):
            if self.verbose:
                print(f"  [{self.role}] Iteration {iteration + 1}")
            
            try:
                response = self.llm.call(msgs, tools=tool_schemas)
            except RuntimeError as e:
                if "400" in str(e) or "tool" in str(e).lower():
                    return None  # LLM doesn't support tool calling
                return f"LLM error: {e}"
            
            tool_calls = response.get("tool_calls", [])
            
            if tool_calls:
                msgs.append(response)
                
                for tc in tool_calls:
                    func_name = tc["function"]["name"]
                    try:
                        args = json.loads(tc["function"]["arguments"])
                    except (json.JSONDecodeError, KeyError):
                        args = {}
                    
                    tool = tool_map.get(func_name)
                    if tool:
                        if self.verbose:
                            print(f"    🔧 {func_name}({list(args.keys())})")
                        result = tool.run(**args)
                    else:
                        result = f"Unknown tool: {func_name}"
                    
                    msgs.append({
                        "role": "tool",
                        "tool_call_id": tc.get("id", f"call_{func_name}"),
                        "content": result[:8000],
                    })
                continue
            
            content = response.get("content", "")
            if content:
                if self.verbose:
                    print(f"  [{self.role}] Done ({len(content)} chars)")
                return content
        
        return f"Agent '{self.role}' reached max iterations"
    
    def _execute_manual(self, messages: list[dict]) -> str:
        """Execute without function calling — include tool descriptions in prompt.
        
        For LLMs that don't support OpenAI tool calling format,
        we describe tools in the system prompt and parse tool usage from text.
        """
        # Add tool descriptions to the system prompt
        tool_desc = "\n\nYou have these tools available. To use a tool, write EXACTLY:\n"
        tool_desc += "TOOL_CALL: tool_name\nARGS: {\"key\": \"value\"}\n\n"
        tool_desc += "Available tools:\n"
        for t in self.tools:
            tool_desc += f"- {t.name}: {t.description}\n"
        tool_desc += "\nWhen done with all tool calls, provide your FINAL ANSWER."
        
        msgs = list(messages)
        msgs[0]["content"] += tool_desc
        
        tool_map = {t.name: t for t in self.tools}
        
        for iteration in range(self.max_iterations):
            if self.verbose:
                print(f"  [{self.role}] Manual iteration {iteration + 1}")
            
            try:
                response = self.llm.call(msgs)
            except RuntimeError as e:
                return f"LLM error: {e}"
            
            content = response.get("content", "")
            
            # Check for tool calls in text
            if "TOOL_CALL:" in content:
                tool_results = self._parse_manual_tool_calls(content, tool_map)
                if tool_results:
                    msgs.append({"role": "assistant", "content": content})
                    msgs.append({"role": "user", "content": f"Tool results:\n{tool_results}\n\nContinue with your task or provide your FINAL ANSWER."})
                    continue
            
            # Check for FINAL ANSWER marker or just return content
            if "FINAL ANSWER" in content:
                # Extract everything after FINAL ANSWER
                idx = content.index("FINAL ANSWER")
                answer = content[idx + len("FINAL ANSWER"):].strip().lstrip(":").strip()
                return answer if answer else content
            
            if content and "TOOL_CALL" not in content:
                return content
        
        return f"Agent '{self.role}' reached max iterations (manual mode)"
    
    def _parse_manual_tool_calls(self, text: str, tool_map: dict) -> str:
        """Parse TOOL_CALL: / ARGS: patterns from text and execute them."""
        results = []
        lines = text.split("\n")
        i = 0
        while i < len(lines):
            line = lines[i].strip()
            if line.startswith("TOOL_CALL:"):
                tool_name = line.split(":", 1)[1].strip()
                args = {}
                if i + 1 < len(lines) and lines[i + 1].strip().startswith("ARGS:"):
                    args_str = lines[i + 1].strip().split(":", 1)[1].strip()
                    try:
                        args = json.loads(args_str)
                    except json.JSONDecodeError:
                        args = {}
                    i += 1
                
                tool = tool_map.get(tool_name)
                if tool:
                    if self.verbose:
                        print(f"    🔧 {tool_name}({list(args.keys())})")
                    result = tool.run(**args)
                    results.append(f"[{tool_name}]: {result[:4000]}")
                else:
                    results.append(f"[{tool_name}]: Unknown tool")
            i += 1
        
        return "\n\n".join(results)
    
    def _execute_simple(self, messages: list[dict]) -> str:
        """Simple execution without tools."""
        try:
            response = self.llm.call(messages)
            return response.get("content", "No response from LLM")
        except RuntimeError as e:
            return f"LLM error: {e}"
    
    def _build_system_prompt(self) -> str:
        """Build the system prompt from role, goal, backstory."""
        parts = [
            f"You are a {self.role}.",
            f"\nYour goal: {self.goal}",
        ]
        if self.backstory:
            parts.append(f"\nBackground: {self.backstory}")
        parts.append("\nProvide thorough, detailed responses. Never use placeholder code or TODOs.")
        return "\n".join(parts)
