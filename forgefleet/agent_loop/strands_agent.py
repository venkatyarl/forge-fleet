"""Strands-based agent that gives local LLMs tool access."""
import json
import subprocess
import os
from pathlib import Path
from typing import Optional


class StrandsAgent:
    """Agent that uses Strands SDK to give local models file/shell access.
    
    Falls back to manual tool execution if Strands tool calling isn't supported
    by the model (not all Qwen versions support OpenAI tool format).
    """
    
    def __init__(self, model_url: str, repo_dir: str, model_name: str = "local"):
        self.model_url = model_url
        self.repo_dir = repo_dir
        self.model_name = model_name
        self._strands_available = self._check_strands()
    
    def _check_strands(self) -> bool:
        """Check if Strands SDK is available."""
        try:
            from strands import Agent
            return True
        except ImportError:
            return False
    
    def run_with_tools(self, task: str, max_iterations: int = 10) -> Optional[str]:
        """Run task with tool-calling agent loop.
        
        If Strands is available and model supports tools: use native tool calling.
        Otherwise: manual agent loop (read files → prompt → parse response → write files).
        """
        if self._strands_available:
            return self._run_strands(task, max_iterations)
        return self._run_manual(task, max_iterations)
    
    def _run_strands(self, task: str, max_iterations: int) -> Optional[str]:
        """Use Strands SDK with llamacpp provider."""
        try:
            from strands import Agent
            from strands.models.llamacpp import LlamaCppModel
            from strands_tools import file_read, file_write, shell, editor
            
            model = LlamaCppModel(base_url=self.model_url)
            
            system_prompt = f"""You are a coding agent working in {self.repo_dir}.
You can read files, write files, and run shell commands.
Always verify your changes compile: run 'cargo check' for Rust, 'npm run build' for TypeScript.
Commit your changes with descriptive messages."""
            
            agent = Agent(
                model=model,
                tools=[file_read, file_write, shell, editor],
                system_prompt=system_prompt,
            )
            
            response = agent(task)
            return str(response)
        except Exception as e:
            print(f"  ⚠️ Strands agent failed: {e}")
            return self._run_manual(task, max_iterations)
    
    def _run_manual(self, task: str, max_iterations: int) -> Optional[str]:
        """Manual agent loop — read files, prompt, parse, write."""
        from forgefleet.agent_loop.file_ops import read_repo_files, write_code_blocks, parse_llm_response
        
        context = read_repo_files(self.repo_dir, max_files=5)
        result = None
        
        for i in range(max_iterations):
            # Build prompt with file context
            prompt = f"""Task: {task}

Current files in repo:
{context}

Instructions:
1. Create or modify files to complete the task
2. Return your changes as code blocks with filenames:
```filename.rs
// code here
```
3. If you need to run a command, say: RUN: <command>
4. When done, say: DONE"""
            
            # Call model
            response = self._call_model(prompt)
            if not response:
                break
            
            # Parse response
            files, commands, done = parse_llm_response(response)
            
            # Write files
            if files:
                write_code_blocks(self.repo_dir, files)
                result = response
            
            # Run commands
            for cmd in commands:
                output = subprocess.run(
                    cmd, shell=True, cwd=self.repo_dir,
                    capture_output=True, text=True, timeout=60
                )
                context += f"\n\nCommand output ({cmd}):\n{output.stdout}\n{output.stderr}"
            
            if done:
                break
            
            # Update context with new file state
            context = read_repo_files(self.repo_dir, max_files=5)
        
        return result
    
    def _call_model(self, prompt: str, max_tokens: int = 4000) -> Optional[str]:
        """Call llama.cpp model via OpenAI-compatible API."""
        try:
            payload = json.dumps({
                "model": "local",
                "messages": [{"role": "user", "content": prompt}],
                "temperature": 0.2,
                "max_tokens": max_tokens,
            })
            r = subprocess.run(
                ["curl", "-s", "--max-time", "900",
                 f"{self.model_url}/v1/chat/completions",
                 "-H", "Content-Type: application/json",
                 "-d", payload],
                capture_output=True, text=True, timeout=910
            )
            if r.returncode == 0:
                resp = json.loads(r.stdout)
                return resp.get("choices", [{}])[0].get("message", {}).get("content", "").strip()
        except:
            pass
        return None
