#!/usr/bin/env bash
#
# ForgeFleet MLX LoRA Training Script
#
# Trains a LoRA adapter on Claude Code conversation data for local fleet LLMs.
# Uses MLX framework for Apple Silicon GPU acceleration.
#
# Usage:
#   ./scripts/train_lora_mlx.sh              # Quick test with Qwen3-8B
#   ./scripts/train_lora_mlx.sh --32b        # Production with Qwen3-32B
#   ./scripts/train_lora_mlx.sh --model X    # Any HuggingFace model
#
set -euo pipefail

# ─── Activate training venv ──────────────────────────────────────────────────
VENV_DIR="$HOME/.forgefleet/training-venv"
if [[ -f "$VENV_DIR/bin/activate" ]]; then
    source "$VENV_DIR/bin/activate"
fi

# ─── Defaults ────────────────────────────────────────────────────────────────
MODEL="${MODEL:-Qwen/Qwen3-8B}"  # Quick test; use --32b for production
DATASET_DIR="$HOME/.forgefleet/training_data"
DATASET_FILE="$DATASET_DIR/dataset.jsonl"
OUTPUT_DIR="$HOME/.forgefleet/lora_adapters/forgefleet-v1"
EPOCHS="${EPOCHS:-3}"
LR="${LR:-2e-4}"
RANK="${RANK:-16}"
BATCH_SIZE="${BATCH_SIZE:-4}"
MAX_SEQ_LEN="${MAX_SEQ_LEN:-4096}"

# ─── Parse args ──────────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case $1 in
        --model)    MODEL="$2";      shift 2 ;;
        --epochs)   EPOCHS="$2";     shift 2 ;;
        --lr)       LR="$2";        shift 2 ;;
        --rank)     RANK="$2";      shift 2 ;;
        --batch)    BATCH_SIZE="$2"; shift 2 ;;
        --32b)      MODEL="Qwen/Qwen3-32B"; shift ;;
        --output)   OUTPUT_DIR="$2"; shift 2 ;;
        --help|-h)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --model MODEL   Base model (default: $MODEL)"
            echo "  --32b           Use Qwen3-32B (production quality)"
            echo "  --epochs N      Training epochs (default: $EPOCHS)"
            echo "  --lr RATE       Learning rate (default: $LR)"
            echo "  --rank N        LoRA rank (default: $RANK)"
            echo "  --batch N       Batch size (default: $BATCH_SIZE)"
            echo "  --output DIR    Adapter output directory"
            echo "  --help          Show this help"
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            exit 1
            ;;
    esac
done

# Version the output dir by model name
MODEL_SHORT=$(echo "$MODEL" | sed 's|.*/||' | tr '[:upper:]' '[:lower:]')
OUTPUT_DIR="$HOME/.forgefleet/lora_adapters/forgefleet-${MODEL_SHORT}-v1"

# ─── Banner ──────────────────────────────────────────────────────────────────
echo "============================================================"
echo "  ForgeFleet LoRA Training (MLX / Apple Silicon)"
echo "============================================================"
echo "  Base Model:   $MODEL"
echo "  Dataset:      $DATASET_FILE"
echo "  Adapter Out:  $OUTPUT_DIR"
echo "  Epochs:       $EPOCHS"
echo "  LR:           $LR"
echo "  LoRA Rank:    $RANK"
echo "  Batch Size:   $BATCH_SIZE"
echo "  Max Seq Len:  $MAX_SEQ_LEN"
echo "============================================================"
echo ""

# ─── Check Python ────────────────────────────────────────────────────────────
if ! command -v python3 &>/dev/null; then
    echo "ERROR: python3 not found. Install Python 3.10+."
    exit 1
fi

# ─── Check/Install mlx-lm ───────────────────────────────────────────────────
echo "[1/5] Checking mlx-lm installation..."
if python3 -c "import mlx_lm" 2>/dev/null; then
    MLX_VERSION=$(python3 -c "import mlx_lm; print(mlx_lm.__version__)" 2>/dev/null || echo "unknown")
    echo "  ✓ mlx-lm $MLX_VERSION installed"
else
    echo "  Installing mlx-lm..."
    pip3 install mlx-lm
    echo "  ✓ mlx-lm installed"
fi

# ─── Check dataset ───────────────────────────────────────────────────────────
echo ""
echo "[2/5] Checking training dataset..."
if [[ ! -f "$DATASET_FILE" ]]; then
    echo "  ✗ Dataset not found at $DATASET_FILE"
    echo "  Run the importer first:"
    echo "    python3 scripts/import_cc_transcripts.py"
    exit 1
fi

EXAMPLE_COUNT=$(wc -l < "$DATASET_FILE" | tr -d ' ')
DATASET_SIZE=$(du -h "$DATASET_FILE" | cut -f1)
echo "  ✓ Dataset: $EXAMPLE_COUNT examples ($DATASET_SIZE)"

if [[ "$EXAMPLE_COUNT" -lt 10 ]]; then
    echo "  ⚠ Only $EXAMPLE_COUNT examples — results may be poor (50+ recommended)"
fi

# ─── Prepare train/valid split ───────────────────────────────────────────────
echo ""
echo "[3/5] Preparing train/valid split..."
TRAIN_FILE="$DATASET_DIR/train.jsonl"
VALID_FILE="$DATASET_DIR/valid.jsonl"

python3 -c "
import random
lines = open('$DATASET_FILE').readlines()
random.seed(42)
random.shuffle(lines)
n_valid = max(1, len(lines) // 10)
with open('$TRAIN_FILE', 'w') as f:
    f.writelines(lines[n_valid:])
with open('$VALID_FILE', 'w') as f:
    f.writelines(lines[:n_valid])
print(f'  Train: {len(lines) - n_valid} examples')
print(f'  Valid: {n_valid} examples')
"

TRAIN_COUNT=$(wc -l < "$TRAIN_FILE" | tr -d ' ')

# ─── Create output directory ─────────────────────────────────────────────────
mkdir -p "$OUTPUT_DIR"

# ─── Run training ────────────────────────────────────────────────────────────
echo ""
echo "[4/5] Starting LoRA training..."
echo "  This will take ~30min for 8B, ~2-4hr for 32B on M2 Ultra."
echo "  Training starts now — $(date)"
echo ""

ITERS=$(( TRAIN_COUNT * EPOCHS ))

python3 -m mlx_lm lora \
    --model "$MODEL" \
    --train \
    --data "$DATASET_DIR" \
    --adapter-path "$OUTPUT_DIR" \
    --iters "$ITERS" \
    --batch-size "$BATCH_SIZE" \
    --num-layers "$RANK" \
    --learning-rate "$LR" \
    --max-seq-length "$MAX_SEQ_LEN" \
    --mask-prompt \
    --grad-checkpoint \
    --seed 42

# ─── Done ────────────────────────────────────────────────────────────────────
echo ""
echo "[5/5] Training complete! — $(date)"
echo "============================================================"
echo ""
echo "  Adapter: $OUTPUT_DIR"
echo ""
echo "  ── Test the adapter ──"
echo ""
echo "  python3 -m mlx_lm generate \\"
echo "    --model $MODEL \\"
echo "    --adapter-path $OUTPUT_DIR \\"
echo "    --prompt 'Read the file at src/main.rs'"
echo ""
echo "  ── Serve as OpenAI-compatible API ──"
echo ""
echo "  python3 -m mlx_lm server \\"
echo "    --model $MODEL \\"
echo "    --adapter-path $OUTPUT_DIR \\"
echo "    --port 51000"
echo ""
echo "  ── Fuse adapter into model (faster inference) ──"
echo ""
echo "  python3 -m mlx_lm fuse \\"
echo "    --model $MODEL \\"
echo "    --adapter-path $OUTPUT_DIR \\"
echo "    --save-path ~/.forgefleet/models/forgefleet-${MODEL_SHORT}-fused"
echo ""
echo "============================================================"
