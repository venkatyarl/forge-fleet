#!/bin/bash
set -uo pipefail

BASH5="/opt/homebrew/bin/bash"
[ -x "$BASH5" ] || BASH5="bash"

PROMPT='Write a bash script that organizes files in the current directory into sub-folders based on the prefix before the first underscore in each filename. For example, report_2024.txt goes into a report folder. Edge cases: 1) files without underscores stay put, 2) hidden files are ignored, 3) if a filename starts with underscore (like _file.txt), leave it alone because the prefix would be empty, 4) avoid creating empty directories. Output ONLY the script content, no markdown, no explanation.'

OUTDIR="/tmp/llm_benchmark_final_v4_$$"
mkdir -p "$OUTDIR/scripts" "$OUTDIR/timings" "$OUTDIR/status"

TESTDIR="$OUTDIR/testdir"
rm -rf "$TESTDIR"
mkdir -p "$TESTDIR"
(
  cd "$TESTDIR"
  touch report_2024.txt report_2025.txt data_jan.csv data_feb.csv
  touch notes.txt readme.md .hidden_file
  touch _leading_underscore.txt no_underscore.txt
)

query_llm() {
    local name="$1" url="$2" model="$3" maxtokens="$4" maxtime="$5"
    local out_script="$OUTDIR/scripts/${name}.sh"
    local out_time="$OUTDIR/timings/${name}.txt"
    local out_status="$OUTDIR/status/${name}.txt"

    local t0=$(date +%s%N)

    local body
    body=$(python3 -c 'import json,sys; print(json.dumps({"model":sys.argv[3],"messages":[{"role":"system","content":"You are a bash scripting expert. Always output ONLY executable bash code. No explanations. No markdown. Start with #!/bin/bash."},{"role":"user","content":sys.argv[6]}],"temperature":0.1,"max_tokens":int(sys.argv[4])}))' "_" "_" "$model" "$maxtokens" "$maxtime" "$PROMPT")

    local raw
    if ! raw=$(curl -s --max-time "$maxtime" -X POST "$url/v1/chat/completions" \
        -H "Content-Type: application/json" -d "$body" 2>/dev/null); then
        echo "ERROR: curl failed or timeout" > "$out_script"
        echo "0" > "$out_time"
        echo "TIMEOUT" > "$out_status"
        return
    fi

    if echo "$raw" | grep -q '"error"'; then
        echo "ERROR: API error" > "$out_script"
        echo "0" > "$out_time"
        echo "API_ERROR" > "$out_status"
        return
    fi

    local script
    script=$(python3 /tmp/extract_script.py "$raw")

    if [ -z "$script" ] || [ "${script#ERROR:}" != "$script" ]; then
        echo "$script" > "$out_script"
        echo "0" > "$out_time"
        echo "EMPTY" > "$out_status"
        return
    fi

    script=$(echo "$script" | sed 's/^```bash//; s/^```sh//; s/^```//; s/```$//; /^```/d')
    echo "$script" > "$out_script"

    local t1=$(date +%s%N)
    local query_ms=$(( (t1 - t0) / 1000000 ))
    echo "$query_ms" > "$out_time"

    local test_copy="$OUTDIR/test_${name}_$$"
    cp -r "$TESTDIR" "$test_copy"
    local ok="PASS"
    (
        cd "$test_copy"
        if $BASH5 "$out_script" >/dev/null 2>&1; then
            [ -d "report" ] && [ "$(ls -1 report/ 2>/dev/null | wc -l | tr -d ' ')" = "2" ] || ok="FAIL"
            [ -d "data" ] && [ "$(ls -1 data/ 2>/dev/null | wc -l | tr -d ' ')" = "2" ] || ok="FAIL"
            [ -f "notes.txt" ] || ok="FAIL"
            [ -f "readme.md" ] || ok="FAIL"
            [ -f ".hidden_file" ] || ok="FAIL"
            [ -f "_leading_underscore.txt" ] || ok="FAIL"
        else
            ok="SYNTAX_ERROR"
        fi
        echo "$ok"
    ) > "$out_status"
    rm -rf "$test_copy"
}

export -f query_llm
export PROMPT OUTDIR TESTDIR BASH5

echo "=== Querying all LLMs in parallel ==="

# Taylor local (only real deployments)
query_llm "taylor-mlx-gemma4"    "http://localhost:55000"         "/Users/venkat/models/gemma-4-31b-it-4bit"       1500 90 &
query_llm "taylor-mlx-qwen36"    "http://localhost:55001"         "/Users/venkat/models/qwen36-35b-a3b"           6000 180 &

# Worker fleet (qwen3.5-9b)
query_llm "ace-qwen3.5-9b"       "http://192.168.5.105:55000"     "qwen3.5-9b"          1500 60 &
query_llm "aura-qwen3.5-9b"      "http://192.168.5.110:55000"     "Qwen3.5-9B-Q4_K_M.gguf" 1500 120 &
query_llm "james-qwen3.5-9b"     "http://192.168.5.108:55000"     "qwen3.5-9b"          1500 60 &

# Worker fleet (qwen3.6-35b)
query_llm "duncan-qwen3.6-35b"   "http://192.168.5.114:55000"     "Qwen3.6-35B-A3B-UD-Q4_K_M.gguf" 2000 120 &
query_llm "lily-qwen3.6-35b"     "http://192.168.5.113:55000"     "Qwen3.6-35B-A3B-UD-Q4_K_M.gguf" 2000 120 &
query_llm "logan-qwen3.6-35b"    "http://192.168.5.111:55000"     "qwen3.6-35b-a3b"     2000 120 &
query_llm "veronica-qwen3.6-35b" "http://192.168.5.112:55000"     "qwen3.6-35b-a3b"     2000 120 &

# Worker fleet (30B+ models)
query_llm "marcus-qwen3-coder"   "http://192.168.5.102:55000"     "Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf" 2000 300 &
query_llm "priya-qwen3-omni"     "http://192.168.5.104:55000"     "Qwen3-Omni-30B-A3B-Instruct-Q4_K_M.gguf" 2000 120 &
query_llm "sophie-qwen3-coder"   "http://192.168.5.103:55000"     "Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf" 2000 120 &

# Worker fleet (deepseek)
query_llm "rihanna-deepseek"     "http://192.168.5.118:55000"     "deepseek-v3.2"       8000 300 &
query_llm "sia-deepseek"         "http://192.168.5.116:55000"     "deepseek-v3.2"       8000 300 &

# Cloud LLMs
query_llm "kimi-code"            "http://localhost:51002"         "kimi-for-coding"     6000 120 &
query_llm "anthropic-oauth"      "http://localhost:51002"         "claude-haiku-4-5-20251001" 1500 30 &

wait
echo "=== All queries complete ==="

echo ""
printf "%-28s %10s %12s\n" "MODEL" "TIME(ms)" "STATUS"
printf "%-28s %10s %12s\n" "----" "--------" "------"

for script in "$OUTDIR/scripts"/*.sh; do
    name=$(basename "$script" .sh)
    time_ms=$(cat "$OUTDIR/timings/${name}.txt" 2>/dev/null || echo "N/A")
    status=$(cat "$OUTDIR/status/${name}.txt" 2>/dev/null || echo "UNKNOWN")
    printf "%-28s %10s %12s\n" "$name" "$time_ms" "$status"
done | sort -k3,3 -k2,2n

echo ""
pass_count=$(grep -l "PASS" "$OUTDIR/status"/*.txt 2>/dev/null | wc -l | tr -d ' ')
total=$(ls "$OUTDIR/status"/*.txt 2>/dev/null | wc -l | tr -d ' ')
echo "PASS: $pass_count / $total"
