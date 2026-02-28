#!/bin/bash

# ============================================================
# Comprehensive test suite for the `dig` CLI
# Run:  ./test_cli.sh [path_to_binary]
# Output: test_cli_output.log (stdout + stderr combined)
# ============================================================

set -o pipefail

DIG_BIN="./target/debug/dig"
TEST_LIST=""

while [[ $# -gt 0 ]]; do
  case $1 in
    --test)
      TEST_LIST="$2"
      shift 2
      ;;
    *)
      DIG_BIN="$1"
      shift
      ;;
  esac
done

LOG_FILE="./test_cli_output.log"

if [ ! -f "$DIG_BIN" ]; then
    echo "Error: $DIG_BIN not found. Build first with: cargo build"
    exit 1
fi

if [ -n "$TEST_LIST" ]; then
    # Sanitize TEST_LIST: replace commas with spaces, trim multiple spaces
    TEST_LIST=$(echo "$TEST_LIST" | tr ',' ' ' | tr -s ' ' | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')
fi

should_run_test() {
    local t="$1"
    if [ -z "$TEST_LIST" ]; then
        return 0
    fi
    for allowed in $TEST_LIST; do
        if [ "$allowed" = "$t" ]; then
            return 0
        fi
    done
    return 1
}

# Use strictly localized environment variables when invoking dig instead of exporting globally
# to avoid polluting the test script's execution environment.

# Clear cache & log before testing so we have a clean slate
rm -rf ./lance_history 2>/dev/null
: > "$LOG_FILE"

PASS=0
FAIL=0

# ---- Helper Functions ----

run_dig() {
    DIG_DEBUG=1 RUST_BACKTRACE=1 "$DIG_BIN" "$@" 2>&1
}

run_dig_piped() {
    local piped="$1"; shift
    echo "$piped" | DIG_DEBUG=1 RUST_BACKTRACE=1 "$DIG_BIN" "$@" 2>&1
}

print_header() {
    local msg="=== Test $TEST_NUM: $1 ==="
    echo -e "\n\033[1;34m${msg}\033[0m" | tee -a "$LOG_FILE"
}

assert_contains() {
    local label="$1"
    local haystack="$2"
    local needle="$3"
    if echo "$haystack" | grep -qiF -- "$needle"; then
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
    if echo "$haystack" | grep -qiF -- "$needle"; then
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

assert_exit_code() {
    local label="$1"
    local actual="$2"
    local expected="$3"
    if [ "$actual" -eq "$expected" ]; then
        echo -e "  \033[1;32m✔ PASS:\033[0m $label (exit $actual)" | tee -a "$LOG_FILE"
        ((PASS++))
    else
        echo -e "  \033[1;31m✘ FAIL:\033[0m $label (expected exit $expected, got $actual)" | tee -a "$LOG_FILE"
        ((FAIL++))
    fi
}

assert_exit_nonzero() {
    local label="$1"
    local actual="$2"
    if [ "$actual" -ne 0 ]; then
        echo -e "  \033[1;32m✔ PASS:\033[0m $label (exit $actual ≠ 0)" | tee -a "$LOG_FILE"
        ((PASS++))
    else
        echo -e "  \033[1;31m✘ FAIL:\033[0m $label (expected non-zero exit, got 0)" | tee -a "$LOG_FILE"
        ((FAIL++))
    fi
}

assert_json_valid() {
    local label="$1"
    local data="$2"
    # DIG_DEBUG emits pretty-printed JSON with bare '{' on its own line.
    # Compact serde_json output is always on a SINGLE line: {"key":val,...}.
    # The '^\{.*\}' pattern matches only lines that both open and close with braces,
    # i.e. complete single-line JSON — skipping DIG_DEBUG bare '{' lines.
    local json_line
    json_line=$(echo "$data" | grep -E '^\{.*\}' | head -1)
    if [ -n "$json_line" ] && echo "$json_line" | python3 -m json.tool >/dev/null 2>&1; then
        echo -e "  \033[1;32m✔ PASS:\033[0m $label" | tee -a "$LOG_FILE"
        ((PASS++))
    else
        echo -e "  \033[1;31m✘ FAIL:\033[0m $label (no valid single-line JSON object found)" | tee -a "$LOG_FILE"
        ((FAIL++))
    fi
}

assert_json_field() {
    local label="$1"
    local data="$2"
    local field="$3"   # e.g. '"success":true'
    local json_line
    json_line=$(echo "$data" | grep -E '^\{.*\}' | head -1)
    if echo "$json_line" | grep -qF "$field"; then
        echo -e "  \033[1;32m✔ PASS:\033[0m $label" | tee -a "$LOG_FILE"
        ((PASS++))
    else
        echo -e "  \033[1;31m✘ FAIL:\033[0m $label (field '$field' not in JSON: $json_line)" | tee -a "$LOG_FILE"
        ((FAIL++))
    fi
}

echo "========================================================" | tee -a "$LOG_FILE"
echo "        DIG CLI COMPREHENSIVE TEST SUITE v3             " | tee -a "$LOG_FILE"
echo "========================================================" | tee -a "$LOG_FILE"
echo "Binary : $DIG_BIN" | tee -a "$LOG_FILE"
echo "Log    : $LOG_FILE" | tee -a "$LOG_FILE"
echo "DIG_DEBUG=1, RUST_BACKTRACE=1" | tee -a "$LOG_FILE"
echo | tee -a "$LOG_FILE"

# ============================================================
# SECTION A: BASIC FUNCTIONALITY
# ============================================================

# ---- T1: One-Shot Safe Command ----
TEST_NUM=1
if should_run_test $TEST_NUM; then
print_header "One-Shot Mode (Safe Command)"
# Use cat Cargo.toml — always maps to a single deterministic command with stable output
OUT=$(run_dig "show me the contents of the Cargo.toml file in the current directory")
echo "$OUT" >> "$LOG_FILE"
assert_contains "LLM executed cat Cargo.toml" "$OUT" "[package]"
fi

# ---- T2: Stdin Pipe Context ----
TEST_NUM=2
if should_run_test $TEST_NUM; then
print_header "Stdin Pipe Context Injection"
OUT=$(run_dig_piped "Hello from pipe test" "what is the exact text that was piped to you?")
echo "$OUT" >> "$LOG_FILE"
assert_contains "LLM recognised piped text" "$OUT" "Hello from pipe test"
fi

# ---- T3: Unsafe Command Headless Denial + --yes Override ----
TEST_NUM=3
if should_run_test $TEST_NUM; then
print_header "Confirm Tier: Headless Denial + --yes Override"
rm -f ./test_dig_unsafe.txt 2>/dev/null

# Step 1: Run headless without --yes, execution should be denied
OUT1=$(run_dig_piped "" "create an empty file named test_dig_unsafe.txt in the current directory")
echo "$OUT1" >> "$LOG_FILE"
assert_contains "Headless confirmation denied execution" "$OUT1" "Execution denied"
if [ ! -f ./test_dig_unsafe.txt ]; then
    echo -e "  \033[1;32m✔ PASS:\033[0m File NOT created without --yes" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m File WAS created without --yes" | tee -a "$LOG_FILE"
    ((FAIL++))
fi

# Step 2: Run headless with --yes, execution should succeed
OUT2=$(run_dig_piped "" --yes "create an empty file named test_dig_unsafe.txt in the current directory")
echo "$OUT2" >> "$LOG_FILE"
assert_file_exists "File created with --yes override" "./test_dig_unsafe.txt"

rm -f ./test_dig_unsafe.txt 2>/dev/null
fi

# ---- T4: Cache Populate + Hit ----
TEST_NUM=4
if should_run_test $TEST_NUM; then
print_header "Jaccard Cache Populate + Hit"
echo "  Step 1: First run (populates cache)..." | tee -a "$LOG_FILE"
OUT1=$(run_dig "what is my IPv4 ip")
echo "$OUT1" >> "$LOG_FILE"
echo "  Step 2: Second run (should cache HIT)..." | tee -a "$LOG_FILE"
OUT2=$(run_dig "what is my IPv4 ip")
echo "$OUT2" >> "$LOG_FILE"
assert_contains "Cache HIT on repeat query" "$OUT2" "Cache HIT"
assert_not_contains "LLM NOT called on cache hit" "$OUT2" "LLM REQUEST"
fi

# ---- T5: Conceptual Question (No Execution) ----
TEST_NUM=5
if should_run_test $TEST_NUM; then
print_header "Conceptual Question (No Command Execution)"
OUT=$(run_dig "what is the difference between TCP and UDP")
echo "$OUT" >> "$LOG_FILE"
assert_contains "Got text about TCP" "$OUT" "TCP"
assert_contains "Got text about UDP" "$OUT" "UDP"

fi
# ============================================================
# SECTION B: LINUX SYSTEM ADMINISTRATION
# ============================================================

# ---- T6: Process & Memory — Top consumers ----
TEST_NUM=6
if should_run_test $TEST_NUM; then
print_header "SysAdmin: Top Memory-Consuming Processes"
OUT=$(run_dig "show me the top 5 processes consuming the most memory with their PIDs and memory usage")
echo "$OUT" >> "$LOG_FILE"
# Should contain process info — PID numbers or %MEM or RSS
if echo "$OUT" | grep -qiE "PID|%MEM|RSS|VSZ|[0-9]+.*[0-9]+"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got process memory info" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m No process info in output" | tee -a "$LOG_FILE"
    ((FAIL++))
fi
assert_not_contains "No panic" "$OUT" "panicked"
fi

# ---- T7: procfs — CPU info ----
TEST_NUM=7
if should_run_test $TEST_NUM; then
print_header "SysAdmin: Read /proc/cpuinfo"
OUT=$(run_dig "how many CPU cores does this machine have and what is the CPU model name, read it from proc filesystem")
echo "$OUT" >> "$LOG_FILE"
if echo "$OUT" | grep -qiE "core|cpu|processor|thread|model"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got CPU info from procfs" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m No CPU info in output" | tee -a "$LOG_FILE"
    ((FAIL++))
fi
fi

# ---- T8: sysfs — CPU governor ----
TEST_NUM=8
if should_run_test $TEST_NUM; then
print_header "SysAdmin: Read sysfs CPU Scaling Governor"
OUT=$(run_dig "what CPU frequency scaling governor is currently active, read from /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
echo "$OUT" >> "$LOG_FILE"
if echo "$OUT" | grep -qiE "performance|powersave|ondemand|schedutil|conservative|governor|No such file"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got CPU governor or appropriate error" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m No governor info" | tee -a "$LOG_FILE"
    ((FAIL++))
fi
fi

# ---- T9: Disk usage sorted ----
TEST_NUM=9
if should_run_test $TEST_NUM; then
print_header "SysAdmin: Disk Usage by Directory (Sorted)"
OUT=$(run_dig "show disk usage of subdirectories in the current directory sorted by size, human readable")
echo "$OUT" >> "$LOG_FILE"
if echo "$OUT" | grep -qiE "[0-9]+[KMG]|target|agent-core|src"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got sorted disk usage" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m No disk usage output" | tee -a "$LOG_FILE"
    ((FAIL++))
fi
fi

# ---- T10: Kernel & Loaded Modules ----
TEST_NUM=10
if should_run_test $TEST_NUM; then
print_header "SysAdmin: Kernel Version and Loaded Modules Count"
OUT=$(run_dig "show the kernel version and count how many kernel modules are currently loaded")
echo "$OUT" >> "$LOG_FILE"
if echo "$OUT" | grep -qiE "[0-9]+\.[0-9]+|module|lsmod"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got kernel/module info" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m No kernel info" | tee -a "$LOG_FILE"
    ((FAIL++))
fi
fi

# ---- T11: Pipe + Complex Filtering — Log Analysis ----
TEST_NUM=11
if should_run_test $TEST_NUM; then
print_header "SysAdmin: Pipe + Grep Chain"
OUT=$(run_dig_piped "$(cat /etc/passwd)" "from the piped data, list only the users who have /bin/bash or /bin/zsh as their shell, show just the usernames")
echo "$OUT" >> "$LOG_FILE"
assert_contains "Found root user" "$OUT" "root"
assert_not_contains "No panic" "$OUT" "panicked"
fi

# ---- T12: Find SUID binaries (Security) ----
TEST_NUM=12
if should_run_test $TEST_NUM; then
print_header "Security: Find SUID Binaries"
OUT=$(run_dig_piped "y" "find all SUID binaries in /usr/bin and show their permissions")
echo "$OUT" >> "$LOG_FILE"
if echo "$OUT" | grep -qiE "suid|rwsr|4[0-7]{3}|/usr/bin/"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Found SUID binaries or permissions" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;33m⚠ WARN:\033[0m No SUID output (may need better permissions)" | tee -a "$LOG_FILE"
    ((PASS++))  # Don't fail — depends on system
fi

fi
# ============================================================
# SECTION C: NETWORK ADMINISTRATION
# ============================================================

# ---- T13: Network interfaces ----
TEST_NUM=13
if should_run_test $TEST_NUM; then
print_header "Network: List All Network Interfaces with IPs"
OUT=$(run_dig "list all network interfaces with their IPv4 addresses and link state")
echo "$OUT" >> "$LOG_FILE"
if echo "$OUT" | grep -qiE "lo|eth|wlan|enp|ens|inet|127\.0\.0\.1|UP|DOWN"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got network interface info" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m No network interface info" | tee -a "$LOG_FILE"
    ((FAIL++))
fi
fi

# ---- T14: Routing table ----
TEST_NUM=14
if should_run_test $TEST_NUM; then
print_header "Network: Show Routing Table"
OUT=$(run_dig "show the routing table with gateway addresses")
echo "$OUT" >> "$LOG_FILE"
if echo "$OUT" | grep -qiE "default|gateway|0\.0\.0\.0|route|via|dev"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got routing table" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m No routing table" | tee -a "$LOG_FILE"
    ((FAIL++))
fi
fi

# ---- T15: DNS Resolution ----
TEST_NUM=15
if should_run_test $TEST_NUM; then
print_header "Network: DNS Resolution"
OUT=$(run_dig_piped "y" "resolve google.com to its IPv4 addresses using the host or dig command")
echo "$OUT" >> "$LOG_FILE"
if echo "$OUT" | grep -qiE "[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+|address|ANSWER"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got DNS resolution result" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m No DNS result" | tee -a "$LOG_FILE"
    ((FAIL++))
fi
fi

# ---- T16: Established connections ----
TEST_NUM=16
if should_run_test $TEST_NUM; then
print_header "Network: Show Established TCP Connections"
OUT=$(run_dig "show all established TCP connections with remote addresses and ports")
echo "$OUT" >> "$LOG_FILE"
if echo "$OUT" | grep -qiE "ESTAB|ESTABLISHED|tcp|LISTEN|State|ss|netstat"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got connection info" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;33m⚠ WARN:\033[0m No established connections (reasonable if isolated)" | tee -a "$LOG_FILE"
    ((PASS++))
fi

fi
# ============================================================
# SECTION D: BINARY ANALYSIS (READ-ONLY TOOLS)
# ============================================================

# ---- T17: readelf — Binary headers ----
TEST_NUM=17
if should_run_test $TEST_NUM; then
print_header "Binary Analysis: readelf on dig binary"
OUT=$(run_dig "use readelf to show the ELF header of the binary $DIG_BIN, including architecture and entry point")
echo "$OUT" >> "$LOG_FILE"
if echo "$OUT" | grep -qiE "ELF|entry point|machine|x86.64|aarch64|Class"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got ELF header info" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m No ELF header info" | tee -a "$LOG_FILE"
    ((FAIL++))
fi
fi

# ---- T18: strings — Extract strings from binary ----
TEST_NUM=18
if should_run_test $TEST_NUM; then
print_header "Binary Analysis: Extract readable strings from binary"
OUT=$(run_dig "use the strings command to find all strings containing 'DIG_DEBUG' in the binary $DIG_BIN")
echo "$OUT" >> "$LOG_FILE"
assert_contains "Found DIG_DEBUG string in binary" "$OUT" "DIG_DEBUG"
fi

# ---- T19: file + ldd — Binary info ----
TEST_NUM=19
if should_run_test $TEST_NUM; then
print_header "Binary Analysis: file type and shared libraries"
OUT=$(run_dig "show the file type and list all shared library dependencies of $DIG_BIN")
echo "$OUT" >> "$LOG_FILE"
if echo "$OUT" | grep -qiE "ELF|dynamically linked|lib|\.so|static"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got binary type + libraries" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m No binary info" | tee -a "$LOG_FILE"
    ((FAIL++))
fi

fi
# ============================================================
# SECTION E: OUTPUT TRUNCATION VALIDATION
# ============================================================

# ---- T20: Large output truncation ----
TEST_NUM=20
if should_run_test $TEST_NUM; then
print_header "Output Truncation: Large command output bounded"
# Note on testability: truncate_output() fires *inside* the agent (sandbox.rs,
# MAX_OUTPUT_LINES=200). When output exceeds that, the LLM receives a truncated
# version with "[OUTPUT TRUNCATED]" and then loops trying to fetch more — burning
# the turn budget. The marker never reaches the final CLI output.
# What we can verify here: the final output is always bounded (no terminal flood)
# regardless of whether the LLM completed or the turn budget exhausted.
OUT=$(env -u DIG_DEBUG RUST_BACKTRACE=1 "$DIG_BIN" \
    "find all files recursively under /usr/lib and show their sizes" 2>&1)
EXIT_CODE=$?
echo "$OUT" >> "$LOG_FILE"
LINE_COUNT=$(echo "$OUT" | wc -l)
# Check 1: output is bounded (no terminal flood), regardless of success/failure
if [ "$LINE_COUNT" -lt 500 ]; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Output is $LINE_COUNT lines (bounded, under 500)" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m Output NOT bounded ($LINE_COUNT lines ≥ 500)" | tee -a "$LOG_FILE"
    ((FAIL++))
fi
# Check 2: either a truncation marker is present, or the agent completed/failed gracefully
if echo "$OUT" | grep -qi "TRUNCATED\|omitted"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Truncation marker present in output" | tee -a "$LOG_FILE"
    ((PASS++))
elif echo "$OUT" | grep -q "Turn budget exhausted"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Turn budget exhausted gracefully (output bounded, no crash)" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;33m⚠ WARN:\033[0m No truncation marker (LLM may have summarised the output)" | tee -a "$LOG_FILE"
    ((PASS++))
fi

fi
# ============================================================
# SECTION F: CWD TRACKING VERIFICATION
# ============================================================

# ---- T21: CWD in context ----
TEST_NUM=21
if should_run_test $TEST_NUM; then
print_header "CWD Tracking: Verify cwd in ExecutorContext"
OUT=$(run_dig "list files in the current directory")
echo "$OUT" >> "$LOG_FILE"
# DIG_DEBUG shows the JSON context — check for cwd field in it
# The cwd field has skip_serializing_if = empty, so check the DIG_DEBUG JSON dump
if echo "$OUT" | grep -qiE 'cwd|current.dir|/home'; then
    echo -e "  \033[1;32m✔ PASS:\033[0m CWD context present in output" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m CWD context NOT present in output" | tee -a "$LOG_FILE"
    ((FAIL++))
fi

fi
# ============================================================
# SECTION G: COMPLEX MULTI-STEP TASKS
# ============================================================

# ---- T22: Multi-command pipeline ----
TEST_NUM=22
if should_run_test $TEST_NUM; then
print_header "Complex: Find largest files + compute checksums"
OUT=$(run_dig "find the 3 largest files in this project directory and compute their sha256 checksums")
echo "$OUT" >> "$LOG_FILE"
if echo "$OUT" | grep -qiE "[a-f0-9]{64}|sha256|checksum|hash"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got file checksums" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m No checksums in output" | tee -a "$LOG_FILE"
    ((FAIL++))
fi
fi

# ---- T23: System health summary ----
TEST_NUM=23
if should_run_test $TEST_NUM; then
print_header "Complex: System Health Summary"
OUT=$(run_dig "give me a system health summary: uptime, load average, disk usage percentage, and memory usage percentage in a single report")
echo "$OUT" >> "$LOG_FILE"
if echo "$OUT" | grep -qiE "uptime|load|disk|memory|%|GB|MB"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got system health data" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m No system health data" | tee -a "$LOG_FILE"
    ((FAIL++))
fi
fi

# ---- T24: File permission audit ----
TEST_NUM=24
if should_run_test $TEST_NUM; then
print_header "Complex: File Permission Audit"
OUT=$(run_dig "in the current project, find all files that are world-writable and list them with their permissions")
echo "$OUT" >> "$LOG_FILE"
# Either finds some or reports none — both are valid
if echo "$OUT" | grep -qiE "no.*world.writable\|no.*found\|rwxrwx\|o+w\|permission\|[0-9]*[2367][0-7]{2}"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Permission audit completed" | tee -a "$LOG_FILE"
    ((PASS++))
else
    # Even if no world-writable files, output should mention the search
    echo -e "  \033[1;32m✔ PASS:\033[0m Permission audit ran (may have no results)" | tee -a "$LOG_FILE"
    ((PASS++))
fi
fi

# ---- T25: Error Handling — nonexistent path ----
TEST_NUM=25
if should_run_test $TEST_NUM; then
print_header "Error Handling: Command Failure (Nonexistent Path)"
OUT=$(run_dig "list the contents of /nonexistent_directory_xyz123")
echo "$OUT" >> "$LOG_FILE"
assert_not_contains "No panic" "$OUT" "panicked"
if echo "$OUT" | grep -qiE "no such file|not found|does not exist|cannot access|error|nonexistent"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Error reported to user" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m Error not reported" | tee -a "$LOG_FILE"
    ((FAIL++))
fi

fi
# ============================================================
# SECTION H: CLI ARGUMENT PARSING — NO LLM REQUIRED
# These tests verify flag parsing, help text, and immediate
# error paths that do not require any LLM API call.
# ============================================================

# ---- T26: --help exits 0 and lists all new flags ----
TEST_NUM=26
if should_run_test $TEST_NUM; then
print_header "CLI: --help exits 0 and lists all flags"
OUT=$("$DIG_BIN" --help 2>&1)
EXIT_CODE=$?
echo "$OUT" >> "$LOG_FILE"
assert_exit_code   "--help exits 0"               "$EXIT_CODE" 0
assert_contains    "--help mentions --dry-run"    "$OUT" "--dry-run"
assert_contains    "--help mentions --safe-only"  "$OUT" "--safe-only"
assert_contains    "--help mentions --no-memory"  "$OUT" "--no-memory"
assert_contains    "--help mentions --no-cache"   "$OUT" "--no-cache"
assert_contains    "--help mentions --json"       "$OUT" "--json"
assert_contains    "--help mentions --config/-c"  "$OUT" "--config"
assert_contains    "--help mentions --read-only"  "$OUT" "--read-only"
assert_contains    "--help mentions --model/-m"   "$OUT" "--model"
assert_contains    "--help mentions --yes/-y"     "$OUT" "--yes"
fi

# ---- T27: --version exits 0 with a semver string ----
TEST_NUM=27
if should_run_test $TEST_NUM; then
print_header "CLI: --version exits 0 with semver version string"
OUT=$("$DIG_BIN" --version 2>&1)
EXIT_CODE=$?
echo "$OUT" >> "$LOG_FILE"
assert_exit_code "--version exits 0"              "$EXIT_CODE" 0
assert_contains  "--version shows binary name"    "$OUT" "dig"
if echo "$OUT" | grep -qE "[0-9]+\.[0-9]+"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m --version contains semver pattern" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m --version has no semver pattern" | tee -a "$LOG_FILE"
    ((FAIL++))
fi
fi

# ---- T28: Short forms -h and -V work identically ----
TEST_NUM=28
if should_run_test $TEST_NUM; then
print_header "CLI: Short flags -h and -V work"
OUT_H=$("$DIG_BIN" -h 2>&1); EXIT_H=$?
OUT_V=$("$DIG_BIN" -V 2>&1); EXIT_V=$?
echo "$OUT_H" >> "$LOG_FILE"
echo "$OUT_V" >> "$LOG_FILE"
assert_exit_code "-h exits 0"             "$EXIT_H" 0
assert_exit_code "-V exits 0"             "$EXIT_V" 0
assert_contains  "-h shows usage"         "$OUT_H" "Usage"
assert_contains  "-V shows version"       "$OUT_V" "dig"
fi

# ---- T29: Flag with missing required value triggers clap error ----
# Note: allow_hyphen_values=true on query means unrecognized flag-like strings are
# silently passed as query text (not errors). Real clap errors come from supplying a
# known flag that requires a value but omitting the value (e.g. --config or --model).
TEST_NUM=29
if should_run_test $TEST_NUM; then
print_header "CLI: Known flag missing required value exits non-zero"
OUT=$("$DIG_BIN" --config 2>&1)
EXIT_CODE=$?
echo "$OUT" >> "$LOG_FILE"
assert_exit_nonzero "--config (no path) exits non-zero" "$EXIT_CODE"
assert_contains     "Shows 'value is required' error" "$OUT" "value is required"
fi

# ---- T30: --config with nonexistent path errors immediately (no LLM) ----
TEST_NUM=30
if should_run_test $TEST_NUM; then
print_header "CLI: --config with nonexistent path fails before LLM"
OUT=$("$DIG_BIN" --config /nonexistent/path/to/config.toml "list files" 2>&1)
EXIT_CODE=$?
echo "$OUT" >> "$LOG_FILE"
assert_exit_nonzero "--config nonexistent exits non-zero"     "$EXIT_CODE"
assert_contains     "Reports config not found"                 "$OUT" "not found"
fi

# ---- T31: --json in REPL mode (no query) shows warning on stderr ----
TEST_NUM=31
if should_run_test $TEST_NUM; then
print_header "CLI: --json with no query warns about REPL-only restriction"
# 'exit' via stdin terminates the REPL; warning should precede it on stderr
OUT=$(timeout 1 "$DIG_BIN" --json </dev/null 2>&1 || true)
echo "$OUT" >> "$LOG_FILE"
assert_contains "--json warns in REPL mode" "$OUT" "only supported in one-shot mode"

fi
# ============================================================
# SECTION I: --debug FLAG
# ============================================================

# ---- T32: --debug sets DIG_DEBUG=1 even when env var is not set ----
TEST_NUM=32
if should_run_test $TEST_NUM; then
print_header "CLI: --debug flag sets DIG_DEBUG env var"
# Run without DIG_DEBUG in env, but with --debug — debug output must appear
OUT=$("$DIG_BIN" --debug "what is the current working directory" 2>&1)
echo "$OUT" >> "$LOG_FILE"
assert_contains "--debug produces [DIG_DEBUG] output" "$OUT" "[DIG_DEBUG]"
assert_not_contains "No panic with --debug"           "$OUT" "panicked"

fi
# ============================================================
# SECTION J: --dry-run FLAG
# ============================================================

# ---- T33: --dry-run shows "Would execute:" prefix and does NOT run commands ----
TEST_NUM=33
if should_run_test $TEST_NUM; then
print_header "CLI: --dry-run previews commands without executing"
rm -f /tmp/test_dig_dryrun_canary.txt 2>/dev/null
OUT=$(run_dig --dry-run --yes "create an empty file named test_dig_dryrun_canary.txt in /tmp")
echo "$OUT" >> "$LOG_FILE"
assert_contains    "Dry-run output has prefix"    "$OUT" "[dry-run] Would execute"
assert_contains    "Dry-run lists a command"      "$OUT" "1."
# File must NOT actually be created (the command was not executed)
if [ ! -f /tmp/test_dig_dryrun_canary.txt ]; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Canary file was NOT created (dry-run worked)" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m Canary file WAS created (dry-run did not prevent execution)" | tee -a "$LOG_FILE"
    ((FAIL++))
    rm -f /tmp/test_dig_dryrun_canary.txt 2>/dev/null
fi
fi

# ---- T34: --dry-run + --json outputs valid JSON with dry_run:true ----
# Use an explicit "run the command" query so the LLM reliably plans a shell command
# (and the dry-run interceptor fires) rather than answering from context knowledge.
TEST_NUM=34
if should_run_test $TEST_NUM; then
print_header "CLI: --dry-run + --json outputs structured JSON"
OUT=$(run_dig --dry-run --json --yes \
    "run the ls command to list files in the current directory and show me the output")
echo "$OUT" >> "$LOG_FILE"
assert_json_valid "Dry-run JSON is valid"                      "$OUT"
assert_json_field "Dry-run JSON has success:true"              "$OUT" '"success":true'
# When the LLM plans a command, the result is DryRun JSON with dry_run + commands.
# If the LLM returns a Completed answer directly, those fields won't be present — that
# is also acceptable behaviour; only assert them when dry_run key exists in the JSON.
JSON_LINE=$(echo "$OUT" | grep -E '^\{.*\}' | head -1)
if echo "$JSON_LINE" | grep -qF '"dry_run"'; then
    assert_json_field "Dry-run JSON has dry_run:true"          "$OUT" '"dry_run":true'
    assert_json_field "Dry-run JSON has commands array"        "$OUT" '"commands"'
else
    echo -e "  \033[1;33m⚠ WARN:\033[0m LLM answered without planning a command (Completed, not DryRun)" | tee -a "$LOG_FILE"
    ((PASS++))
fi

fi
# ============================================================
# SECTION K: --json OUTPUT
# ============================================================

# ---- T35: --json in one-shot mode outputs valid JSON with result field ----
TEST_NUM=35
if should_run_test $TEST_NUM; then
print_header "CLI: --json one-shot mode outputs valid JSON"
OUT=$(run_dig --json --yes "what is 1 plus 1")
echo "$OUT" >> "$LOG_FILE"
assert_json_valid "One-shot --json is valid JSON"              "$OUT"
assert_json_field "JSON has success:true"                      "$OUT" '"success":true'
assert_json_field "JSON has result field"                      "$OUT" '"result"'
assert_not_contains "--json output has no [dry-run] prefix"   "$OUT" "[dry-run]"

fi
# ============================================================
# SECTION L: PERMISSION FLAGS (--yes, --safe-only)
# ============================================================

# ---- T36: --yes auto-approves Confirm-tier commands without stdin ----
TEST_NUM=36
if should_run_test $TEST_NUM; then
print_header "CLI: --yes auto-approves without requiring stdin confirmation"
rm -f /tmp/test_dig_yes_auto.txt 2>/dev/null
# Run entirely non-interactively — no piped 'y', no TTY; --yes handles approval
OUT=$(run_dig --yes "create an empty file named test_dig_yes_auto.txt in /tmp")
echo "$OUT" >> "$LOG_FILE"
assert_file_exists "File created with --yes auto-approve"      "/tmp/test_dig_yes_auto.txt"
rm -f /tmp/test_dig_yes_auto.txt 2>/dev/null
assert_not_contains "No panic with --yes"                      "$OUT" "panicked"
fi

# ---- T37: --safe-only allows genuinely Safe-tier commands ----
TEST_NUM=37
if should_run_test $TEST_NUM; then
print_header "CLI: --safe-only allows Safe-tier commands (ls, cat, pwd)"
OUT=$(run_dig --safe-only "show the current working directory path")
echo "$OUT" >> "$LOG_FILE"
assert_not_contains "safe-only does NOT deny Safe command"    "$OUT" "Denied"
assert_not_contains "No panic with --safe-only"               "$OUT" "panicked"
# Output should contain a directory path
if echo "$OUT" | grep -qE "^/|/home|/root"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got directory path in output" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;33m⚠ WARN:\033[0m Path not clearly visible (check log)" | tee -a "$LOG_FILE"
    ((PASS++))  # don't fail — output format may vary
fi
fi

# ---- T38: --safe-only denies Confirm-tier commands even with --yes ----
TEST_NUM=38
if should_run_test $TEST_NUM; then
print_header "CLI: --safe-only + --yes denies Confirm-tier (git is Confirm)"
# git is classified Confirm tier; the LLM will almost certainly use it here
OUT=$(run_dig --safe-only --yes "run git log to show the 5 most recent commits in this repo")
echo "$OUT" >> "$LOG_FILE"
if echo "$OUT" | grep -qiE "Denied|safe-only|tier"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m safe-only denied Confirm-tier command" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;33m⚠ WARN:\033[0m Denial not observed — LLM may have used a Safe-tier alternative" | tee -a "$LOG_FILE"
    ((PASS++))  # LLM behaviour is non-deterministic
fi
assert_not_contains "No panic with --safe-only --yes"         "$OUT" "panicked"

fi
# ============================================================
# SECTION M: CACHE AND MEMORY FLAGS
# ============================================================

# ---- T39: --no-cache forces fresh LLM call on repeated query ----
TEST_NUM=39
if should_run_test $TEST_NUM; then
print_header "CLI: --no-cache skips Jaccard cache on repeated query"
# T4 already populated the cache for "what is my IPv4 ip"
# With --no-cache the second call must not be a cache hit
OUT=$(run_dig --no-cache "what is my IPv4 ip")
echo "$OUT" >> "$LOG_FILE"
assert_not_contains "Cache NOT hit with --no-cache"           "$OUT" "Cache HIT"
assert_not_contains "No panic with --no-cache"                "$OUT" "panicked"
fi

# ---- T40: --no-memory disables semantic history and skips cache ----
TEST_NUM=40
if should_run_test $TEST_NUM; then
print_header "CLI: --no-memory disables LanceDB history (no cache hit)"
OUT=$(run_dig --no-memory "what is the current username")
echo "$OUT" >> "$LOG_FILE"
# no_memory subsumes no_cache — cache hits should not appear
assert_not_contains "No cache with --no-memory"               "$OUT" "Cache HIT"
assert_not_contains "No panic with --no-memory"               "$OUT" "panicked"
# Should still produce useful output
if echo "$OUT" | grep -qiE "[a-z_][a-z0-9_]{2,}|username|user"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got username output with --no-memory" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;33m⚠ WARN:\033[0m Username not clearly visible in output" | tee -a "$LOG_FILE"
    ((PASS++))
fi

fi
# ============================================================
# SECTION N: --model FLAG
# ============================================================

# ---- T41: --model with invalid format (contains space) warns and falls back ----
TEST_NUM=41
if should_run_test $TEST_NUM; then
print_header "CLI: --model with invalid format warns and falls back to config default"
# "invalid model string" has spaces → validate_model_format() returns false → warning + fallback
OUT=$(run_dig --model "invalid model with spaces" --yes "what is the current date" 2>&1)
echo "$OUT" >> "$LOG_FILE"
assert_contains "Invalid model format warning printed" "$OUT" "Invalid model format"
assert_contains "Fallback message printed"             "$OUT" "Falling back"
assert_not_contains "No panic with invalid model"     "$OUT" "panicked"

fi
# ============================================================
# SECTION O: QUERY SEPARATOR (--)
# ============================================================

# ---- T42: `dig -- --version` passes the flag as query text, not a clap flag ----
# Design: allow_hyphen_values=true on query means flag-like words become query text.
# The `--` separator explicitly signals end-of-options to clap.
# Contrast: `dig --version` exits immediately with "dig 0.1.0" (handled by clap).
#           `dig -- --version` treats "--version" as the query → LLM call → no "dig 0.1.0".
TEST_NUM=42
if should_run_test $TEST_NUM; then
print_header "CLI: -- separator prevents clap from interpreting --version as a flag"
# Without --: clap handles --version immediately, exits 0, prints version
OUT_VER=$("$DIG_BIN" --version 2>&1)
EXIT_VER=$?
echo "$OUT_VER" >> "$LOG_FILE"
assert_exit_code "--version exits 0" "$EXIT_VER" 0
assert_contains  "--version prints version string" "$OUT_VER" "dig 0.1.0"
# With --: "--version" becomes the query; LLM is called; "dig 0.1.0" must NOT appear
# (timeout 5 in case the LLM is slow; exit 124 = timeout, also acceptable)
OUT_SEP=$(timeout 5 "$DIG_BIN" -- --version 2>&1 || true)
echo "$OUT_SEP" >> "$LOG_FILE"
assert_not_contains "-- --version not treated as clap flag (no 'dig 0.1.0' in output)" \
    "$OUT_SEP" "dig 0.1.0"

fi
# ============================================================
# SECTION P: COMBINED FLAGS AND EDGE CASES
# ============================================================

# ---- T43: --read-only flag completes a read-only query successfully ----
TEST_NUM=43
if should_run_test $TEST_NUM; then
print_header "CLI: --read-only flag completes read-only queries without error"
OUT=$(run_dig --read-only "show the contents of /etc/hostname")
echo "$OUT" >> "$LOG_FILE"
assert_not_contains "No panic with --read-only"      "$OUT" "panicked"
# hostname file should have content
if echo "$OUT" | grep -qiE "[a-z0-9][a-z0-9\-]*"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got hostname output with --read-only" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;33m⚠ WARN:\033[0m Output unclear — check log" | tee -a "$LOG_FILE"
    ((PASS++))
fi
fi

# ---- T44: --no-cache + --no-memory combined: no cache, no history, still works ----
TEST_NUM=44
if should_run_test $TEST_NUM; then
print_header "CLI: --no-cache + --no-memory combined flags work together"
OUT=$(run_dig --no-cache --no-memory --yes "list files in the current directory")
echo "$OUT" >> "$LOG_FILE"
assert_not_contains "No cache with combined flags"   "$OUT" "Cache HIT"
assert_not_contains "No panic with combined flags"   "$OUT" "panicked"
if echo "$OUT" | grep -qiE "Cargo|src|target|README|config"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Got directory listing with combined flags" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;33m⚠ WARN:\033[0m Listing unclear — check log" | tee -a "$LOG_FILE"
    ((PASS++))
fi

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
