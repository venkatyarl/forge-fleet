---
name: model-arbitrage
description: |
  Per-task router that picks the cheapest model capable of doing
  the work. Considers 23 cloud providers + 7 local LLM tiers and
  produces a routing decision with the dollar receipt. The local
  fleet wins almost every well-scoped task at $0 marginal.
when-to-invoke: |
  Before any cloud LLM call. If the task is well-scoped — code edit,
  classification, summary, JSON extraction — there is almost always
  a local model that can do it for free.
family: routing
source: forgefleet
version: 1.0.0
tools:
  - Bash
---

# Model arbitrage

The point of ForgeFleet is that a Mac Studio + 14 commodity boxes
amortize over years. Most coding tasks don't need GPT-5 or Opus.

## How to route

```bash
# Print the routing decision for a task without dispatching:
ff cloud-llm arbitrage "<task description>" --explain

# Dispatch via the chosen route:
ff run "<task>"   # default cascade
ff supervise "<task>"   # tiered with retry
```

## Defaults (May 2026)

| Task shape | Default route | Why |
|------------|---------------|-----|
| One-shot text | Taylor mlx (Qwen3.5-35B) | Lowest latency on leader |
| Code edit | Marcus Qwen3-Coder-30B | Best local tool-caller |
| Code review | Sophie Qwen3-Coder-30B + Sia Qwen3-30B (debate) | Two models cheaper than one cloud call |
| Long-horizon plan | James Qwen3.5-72B | More headroom for chain-of-thought |
| Hard reasoning (last resort) | claude-opus / gpt-5 | Only when local is provably stuck |

## What this owns

- Refuses to call cloud if any local model can do it (configurable).
- Logs the saved-$/call to `skill_invocations.cost_usd`.
- Shows the saved $ in `ff pulse`.
