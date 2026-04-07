# ForgeFleet Inference Engine Support

## Currently Implemented (in ff-runtime)
| Engine | Status | Best For |
|--------|--------|----------|
| **llama.cpp** | ✅ Full | CPU/Metal/CUDA, GGUF models, RPC distributed |
| **vLLM** | ✅ Full | NVIDIA GPU, high throughput, continuous batching |
| **MLX** | ✅ Full | Apple Silicon, fast local inference |
| **Ollama** | ✅ Full | Universal fallback, easy model management |
| **TensorRT-LLM** | 🟡 Stub | NVIDIA max performance (needs implementation) |

## Recommended Additions
| Engine | Priority | Why |
|--------|----------|-----|
| **mistral.rs** | HIGH | Rust native, no Python, ISQ on-the-fly quantization, single binary |
| **Exo** | MEDIUM | P2P distributed inference across heterogeneous nodes (run 405B on fleet) |
| **TensorRT-LLM** | MEDIUM | Max throughput on DGX Sparks (compile models to TRT engines) |

## Skip
| Engine | Why |
|--------|-----|
| **AirLLM** | Too slow (minutes per token). Fleet architecture already solves the "not enough RAM" problem |

## Fleet Hardware → Engine Mapping
| Node Type | RAM | Best Engine |
|-----------|-----|-------------|
| DGX Spark (×4) | 128GB each | vLLM (tensor parallel) or TensorRT-LLM |
| GMKtec EVO-X2 (×4) | 128GB each | llama.cpp (ROCm/Vulkan) or mistral.rs |
| Mac Studio (Taylor) | 96GB | MLX or llama.cpp (Metal) |
| Mac mini M4 (Ace) | 16GB | llama.cpp (Metal) |
| Intel boxes (Marcus/Sophie/Priya) | 32GB each | llama.cpp (CPU) |
| Mac mini Intel (James) | 64GB | llama.cpp (CPU) |

## Distributed Inference Strategy
- **Within DGX Spark group**: Tensor parallel (fast NVLink-like interconnect)
- **Across EVO-X2 nodes**: Pipeline parallel via llama.cpp RPC or Exo
- **Mixed fleet**: Route by model size — small models on individual nodes, large models sharded across multiple
