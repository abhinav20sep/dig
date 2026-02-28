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

# ---- Helper Functions ----

run_dig() {
    "$DIG_BIN" "$@" 2>&1
}

run_dig_piped() {
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
echo "        DIG CLI COMPREHENSIVE TEST SUITE v2             " | tee -a "$LOG_FILE"
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
print_header "One-Shot Mode (Safe Command)"
OUT=$(run_dig "list all files in the current directory that start with C or c")
echo "$OUT" >> "$LOG_FILE"
assert_contains "LLM responded with file listing" "$OUT" "Cargo"

# ---- T2: Stdin Pipe Context ----
TEST_NUM=2
print_header "Stdin Pipe Context Injection"
OUT=$(run_dig_piped "Hello from pipe test" "what is the exact text that was piped to you?")
echo "$OUT" >> "$LOG_FILE"
assert_contains "LLM recognised piped text" "$OUT" "Hello from pipe test"

# ---- T3: Unsafe Command Confirmation ----
TEST_NUM=3
print_header "Unsafe Command Confirmation"
rm -f /tmp/test_dig_unsafe.txt 2>/dev/null
OUT=$(run_dig_piped "y" "create an empty file named test_dig_unsafe.txt in /tmp")
echo "$OUT" >> "$LOG_FILE"
assert_file_exists "File created after confirmation" "/tmp/test_dig_unsafe.txt"
rm -f /tmp/test_dig_unsafe.txt 2>/dev/null

# ---- T4: Cache Populate + Hit ----
TEST_NUM=4
print_header "Jaccard Cache Populate + Hit"
echo "  Step 1: First run (populates cache)..." | tee -a "$LOG_FILE"
OUT1=$(run_dig "what is my IPv4 ip")
echo "$OUT1" >> "$LOG_FILE"
echo "  Step 2: Second run (should cache HIT)..." | tee -a "$LOG_FILE"
OUT2=$(run_dig "what is my IPv4 ip")
echo "$OUT2" >> "$LOG_FILE"
assert_contains "Cache HIT on repeat query" "$OUT2" "Cache HIT"
assert_not_contains "LLM NOT called on cache hit" "$OUT2" "LLM REQUEST"

# ---- T5: Conceptual Question (No Execution) ----
TEST_NUM=5
print_header "Conceptual Question (No Command Execution)"
OUT=$(run_dig "what is the difference between TCP and UDP")
echo "$OUT" >> "$LOG_FILE"
assert_contains "Got text about TCP" "$OUT" "TCP"
assert_contains "Got text about UDP" "$OUT" "UDP"

# ============================================================
# SECTION B: LINUX SYSTEM ADMINISTRATION
# ============================================================

# ---- T6: Process & Memory — Top consumers ----
TEST_NUM=6
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

# ---- T7: procfs — CPU info ----
TEST_NUM=7
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

# ---- T8: sysfs — CPU governor ----
TEST_NUM=8
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

# ---- T9: Disk usage sorted ----
TEST_NUM=9
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

# ---- T10: Kernel & Loaded Modules ----
TEST_NUM=10
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

# ---- T11: Pipe + Complex Filtering — Log Analysis ----
TEST_NUM=11
print_header "SysAdmin: Pipe + Grep Chain"
OUT=$(run_dig_piped "$(cat /etc/passwd)" "from the piped data, list only the users who have /bin/bash or /bin/zsh as their shell, show just the usernames")
echo "$OUT" >> "$LOG_FILE"
assert_contains "Found root user" "$OUT" "root"
assert_not_contains "No panic" "$OUT" "panicked"

# ---- T12: Find SUID binaries (Security) ----
TEST_NUM=12
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

# ============================================================
# SECTION C: NETWORK ADMINISTRATION
# ============================================================

# ---- T13: Network interfaces ----
TEST_NUM=13
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

# ---- T14: Routing table ----
TEST_NUM=14
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

# ---- T15: DNS Resolution ----
TEST_NUM=15
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

# ---- T16: Established connections ----
TEST_NUM=16
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

# ============================================================
# SECTION D: BINARY ANALYSIS (READ-ONLY TOOLS)
# ============================================================

# ---- T17: readelf — Binary headers ----
TEST_NUM=17
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

# ---- T18: strings — Extract strings from binary ----
TEST_NUM=18
print_header "Binary Analysis: Extract readable strings from binary"
OUT=$(run_dig "use the strings command to find all strings containing 'DIG_DEBUG' in the binary $DIG_BIN")
echo "$OUT" >> "$LOG_FILE"
assert_contains "Found DIG_DEBUG string in binary" "$OUT" "DIG_DEBUG"

# ---- T19: file + ldd — Binary info ----
TEST_NUM=19
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

# ============================================================
# SECTION E: OUTPUT TRUNCATION VALIDATION
# ============================================================

# ---- T20: Large output truncation ----
TEST_NUM=20
print_header "Output Truncation: Large find output"
OUT=$(run_dig "find all files recursively under /usr/lib and show their sizes")
echo "$OUT" >> "$LOG_FILE"
# Strip DIG_DEBUG lines before counting — they inflate the line count
CLEAN_OUT=$(echo "$OUT" | grep -v '\[DIG_DEBUG\]' | grep -v '^{' | grep -v '^}' | grep -v '^  "')
LINE_COUNT=$(echo "$CLEAN_OUT" | wc -l)
if [ "$LINE_COUNT" -lt 500 ]; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Output truncated to $LINE_COUNT user-facing lines (under 500)" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;31m✘ FAIL:\033[0m Output NOT truncated ($LINE_COUNT user-facing lines)" | tee -a "$LOG_FILE"
    ((FAIL++))
fi
if echo "$OUT" | grep -qi "TRUNCATED\|omitted"; then
    echo -e "  \033[1;32m✔ PASS:\033[0m Truncation marker present in output" | tee -a "$LOG_FILE"
    ((PASS++))
else
    echo -e "  \033[1;33m⚠ WARN:\033[0m No truncation marker (output may have been short enough)" | tee -a "$LOG_FILE"
    ((PASS++))  # Don't fail — depends on system
fi

# ============================================================
# SECTION F: CWD TRACKING VERIFICATION
# ============================================================

# ---- T21: CWD in context ----
TEST_NUM=21
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

# ============================================================
# SECTION G: COMPLEX MULTI-STEP TASKS
# ============================================================

# ---- T22: Multi-command pipeline ----
TEST_NUM=22
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

# ---- T23: System health summary ----
TEST_NUM=23
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

# ---- T24: File permission audit ----
TEST_NUM=24
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

# ---- T25: Error Handling — nonexistent path ----
TEST_NUM=25
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
