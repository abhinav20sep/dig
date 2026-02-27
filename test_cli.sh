#!/bin/bash

# ============================================================
# Comprehensive test suite for the `dig` CLI
# Run:  ./test_cli.sh [path_to_binary]
# Output: test_cli_output.log (stdout + stderr combined)
# ============================================================

set -o pipefail

DIG_BIN=${1:-"./target/debug/dig"}
LOG_FILE="./test_cli_output.log"

if [ ! -f "$DIG_BIN" ]; then
    echo "Error: $DIG_BIN not found. Build first with: cargo build"
    exit 1
fi

export DIG_DEBUG=1
export RUST_BACKTRACE=1

# Clear cache & log before testing so we have a clean slate
rm -rf ./lance_history 2>/dev/null
: > "$LOG_FILE"

PASS=0
FAIL=0
WARN=0

# ---- Helper Functions ----

# Redirect both stdout+stderr to log and terminal
run_dig() {
    "$DIG_BIN" "$@" 2>&1
}

run_dig_piped() {
    # $1 = piped text, rest = dig args
    local piped="$1"; shift
    echo "$piped" | "$DIG_BIN" "$@" 2>&1
}

print_header() {
    local msg="=== Test $TEST_NUM: $1 ==="
    echo -e "\n\033[1;34m${msg}\033[0m" | tee -a "$LOG_FILE"
}

assert_contains() {
    local label="$1"
    local haystack="$2"
    local needle="$3"
    if echo "$haystack" | grep -qiF "$needle"; then
        echo -e "  \033[1;32m✔ PASS:\033[0m $label" | tee -a "$LOG_FILE"
        ((PASS++))
    else
        echo -e "  \033[1;31m✘ FAIL:\033[0m $label (expected '$needle')" | tee -a "$LOG_FILE"
        ((FAIL++))
    fi
}

assert_not_contains() {
    local label="$1"
    local haystack="$2"
    local needle="$3"
    if echo "$haystack" | grep -qiF "$needle"; then
        echo -e "  \033[1;31m✘ FAIL:\033[0m $label (should NOT contain '$needle')" | tee -a "$LOG_FILE"
        ((FAIL++))
    else
        echo -e "  \033[1;32m✔ PASS:\033[0m $label" | tee -a "$LOG_FILE"
        ((PASS++))
    fi
}

assert_file_exists() {
    local label="$1"
    local path="$2"
    if [ -e "$path" ]; then
        echo -e "  \033[1;32m✔ PASS:\033[0m $label ($path exists)" | tee -a "$LOG_FILE"
        ((PASS++))
    else
        echo -e "  \033[1;31m✘ FAIL:\033[0m $label ($path not found)" | tee -a "$LOG_FILE"
        ((FAIL++))
    fi
}

echo "========================================================" | tee -a "$LOG_FILE"
echo "          DIG CLI COMPREHENSIVE TEST SUITE              " | tee -a "$LOG_FILE"
echo "========================================================" | tee -a "$LOG_FILE"
echo "Binary : $DIG_BIN" | tee -a "$LOG_FILE"
echo "Log    : $LOG_FILE" | tee -a "$LOG_FILE"
echo "DIG_DEBUG=1, RUST_BACKTRACE=1" | tee -a "$LOG_FILE"
echo | tee -a "$LOG_FILE"

# ============================================================
# T1: One-Shot Mode — Safe read-only command
# ============================================================
TEST_NUM=1
print_header "One-Shot Mode (Safe Command)"
OUT=$(run_dig "list all files in the current directory that start with C or c")
echo "$OUT" >> "$LOG_FILE"
assert_contains "LLM responded with file listing output" "$OUT" "Cargo"

# ============================================================
# T2: Stdin Pipe Context Injection
# ============================================================
TEST_NUM=2
print_header "Stdin Pipe Context Injection"
OUT=$(run_dig_piped "Hello from pipe test" "what is the exact text that was piped to you?")
echo "$OUT" >> "$LOG_FILE"
assert_contains "LLM recognised piped text" "$OUT" "Hello from pipe test"

# ============================================================
# T3: Unsafe Command — requires confirmation
# ============================================================
TEST_NUM=3
print_header "Unsafe Command Confirmation"
rm -f /tmp/test_dig_unsafe.txt 2>/dev/null
OUT=$(run_dig_piped "y" "create an empty file named test_dig_unsafe.txt in /tmp")
echo "$OUT" >> "$LOG_FILE"
assert_file_exists "Destructive command file created" "/tmp/test_dig_unsafe.txt"
rm -f /tmp/test_dig_unsafe.txt 2>/dev/null

# ============================================================
# T4: Jaccard Cache — Populate then HIT (exact same query)
# ============================================================
TEST_NUM=4
print_header "Jaccard Cache Populate + Hit"
echo "  Step 1: First run (populates cache)..." | tee -a "$LOG_FILE"
OUT1=$(run_dig "what is my IPv4 ip")
echo "$OUT1" >> "$LOG_FILE"
assert_contains "First run: got an IP address" "$OUT1" "192.168"

echo "  Step 2: Second run (should cache HIT)..." | tee -a "$LOG_FILE"
OUT2=$(run_dig "what is my IPv4 ip")
echo "$OUT2" >> "$LOG_FILE"
assert_contains "Second run: Cache HIT debug line present" "$OUT2" "Cache HIT"
assert_not_contains "Second run: LLM was NOT called" "$OUT2" "LLM REQUEST"

# ============================================================
# T5: Jaccard Cache — MISS for different query
# ============================================================
TEST_NUM=5
print_header "Jaccard Cache Miss (Different Query)"
OUT=$(run_dig "show me all active network interfaces")
echo "$OUT" >> "$LOG_FILE"
assert_not_contains "Different query did NOT hit IPv4 cache" "$OUT" "Cache HIT"
assert_contains "LLM was called for new query" "$OUT" "LLM REQUEST"

# ============================================================
# T6: Jaccard Cache — Near-miss (similar but different intent)
# ============================================================
TEST_NUM=6
print_header "Jaccard Cache Near-Miss (Similar Words, Different Intent)"
OUT=$(run_dig "what is the IPv4 address of google.com")
echo "$OUT" >> "$LOG_FILE"
# "what is my IPv4 ip" vs "what is the IPv4 address of google.com"
# Jaccard should NOT match — different intent (my ip vs google.com)
assert_contains "LLM was called (not a false cache hit)" "$OUT" "LLM REQUEST"

# ============================================================
# T7: Empty query handling
# ============================================================
TEST_NUM=7
print_header "Empty Query Handling"
OUT=$(run_dig "" 2>&1 || true)
echo "$OUT" >> "$LOG_FILE"
# Should either start REPL or show help — should NOT crash
assert_not_contains "No panic or crash" "$OUT" "panicked"

# ============================================================
# T8: Multi-word natural language (complex query)
# ============================================================
TEST_NUM=8
print_header "Complex Natural Language Query"
OUT=$(run_dig "find all rust source files larger than 10KB in this project and sort them by size descending")
echo "$OUT" >> "$LOG_FILE"
assert_contains "LLM executed a find/sort command" "$OUT" ".rs"

# ============================================================
# T9: Error handling — command that fails
# ============================================================
TEST_NUM=9
print_header "Error Handling (Command Failure)"
OUT=$(run_dig "list the contents of /nonexistent_directory_xyz123")
echo "$OUT" >> "$LOG_FILE"
# Should handle the error gracefully — report it, not crash
assert_not_contains "No panic" "$OUT" "panicked"
# Check error was reported (any of these phrases)
if echo "$OUT" | grep -qiE "no such file|not found|does not exist|cannot access|error|nonexistent"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Error reported to user" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m Error not reported to user" | tee -a "$LOG_FILE"
    ((FAIL++))
fi

# ============================================================
# T10: Pipe position head — raw output for downstream
# ============================================================
TEST_NUM=10
print_header "Pipe Head Position (Machine-Readable Output)"
OUT=$(run_dig "what is 2+2" | cat)
echo "$OUT" >> "$LOG_FILE"
assert_contains "Got a numeric answer" "$OUT" "4"

# ============================================================
# T11: Conceptual question — should NOT execute commands
# ============================================================
TEST_NUM=11
print_header "Conceptual Question (No Command Execution)"
OUT=$(run_dig "what is the difference between TCP and UDP")
echo "$OUT" >> "$LOG_FILE"
# Should answer with text, not execute bash commands
assert_contains "Got a text explanation" "$OUT" "TCP"
assert_contains "UDP mentioned" "$OUT" "UDP"

# ============================================================
# T12: Repeated cache hit returns consistent answer
# ============================================================
TEST_NUM=12
print_header "Cache Consistency (Same Answer on Repeat)"
OUT1=$(run_dig "what is my IPv4 ip" 2>/dev/null | grep -v "DIG_DEBUG")
OUT2=$(run_dig "what is my IPv4 ip" 2>/dev/null | grep -v "DIG_DEBUG")
echo "Run 1: $OUT1" >> "$LOG_FILE"
echo "Run 2: $OUT2" >> "$LOG_FILE"
if [ "$OUT1" = "$OUT2" ]; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Cached response is identical across runs" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m Cached responses differ:\n    Run1='$OUT1'\n    Run2='$OUT2'" | tee -a "$LOG_FILE"
    ((FAIL++))
fi

# ============================================================
# SUMMARY
# ============================================================
echo | tee -a "$LOG_FILE"
echo "========================================================" | tee -a "$LOG_FILE"
TOTAL=$((PASS + FAIL))
echo -e "  Results: \033[1;32m${PASS} passed\033[0m / \033[1;31m${FAIL} failed\033[0m / ${TOTAL} total" | tee -a "$LOG_FILE"
echo "========================================================" | tee -a "$LOG_FILE"

if [ "$FAIL" -gt 0 ]; then
    echo -e "\033[1;31mSome tests FAILED. Review $LOG_FILE for details.\033[0m" | tee -a "$LOG_FILE"
    exit 1
else
    echo -e "\033[1;32mAll tests PASSED!\033[0m" | tee -a "$LOG_FILE"
    exit 0
fi
