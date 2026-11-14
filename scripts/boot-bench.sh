#!/usr/bin/env bash
# Compare kernel boot time under tvm and under QEMU's microvm machine.
#
# Both boot the same kernel with the same command line and no root
# filesystem. Such a boot ends in a panic and a machine reset, at which point
# either VMM exits. The time reported is therefore the wall-clock life of the
# whole process: VMM start, kernel boot, reset.
#
# usage: scripts/boot-bench.sh [kernel]
#   RUNS=10   boots per VMM
#   MEM=512   guest memory in MiB
#
# Numbers taken inside WSL2 are nested-virtualisation numbers: every VM exit
# costs several times what it does on bare-metal Linux. Compare VMMs on the
# same host, and take absolute figures on bare metal.

set -euo pipefail

KERNEL=${1:-guest/vmlinuz-virt}
RUNS=${RUNS:-10}
MEM=${MEM:-512}
CMDLINE="console=ttyS0 reboot=k panic=-1 pci=off"

cd "$(dirname "$0")/.."

if [[ ! -r $KERNEL ]]; then
    echo "kernel '$KERNEL' not found; see the README for how to fetch one" >&2
    exit 1
fi

echo "building tvm (release)..."
cargo build --release --quiet
TVM=target/release/vmm

# Run a command with its output discarded; print how long it took in ms.
time_ms() {
    local start end
    start=$(date +%s%N)
    "$@" </dev/null >/dev/null 2>&1 || return 1
    end=$(date +%s%N)
    echo $(((end - start) / 1000000))
}

# bench <name> <command...>: boot RUNS times, print min / median / max.
bench() {
    local name=$1 t i
    shift
    local times=()
    for ((i = 0; i < RUNS; i++)); do
        if ! t=$(time_ms "$@"); then
            printf '%-14s run %d failed; run it by hand to see why:\n  %s\n' \
                "$name" "$((i + 1))" "$*"
            return 0
        fi
        times+=("$t")
    done
    local sorted
    mapfile -t sorted < <(printf '%s\n' "${times[@]}" | sort -n)
    printf '%-14s min %5d ms   median %5d ms   max %5d ms\n' \
        "$name" "${sorted[0]}" "${sorted[$((RUNS / 2))]}" "${sorted[$((RUNS - 1))]}"
}

echo "kernel $KERNEL, $MEM MiB, $RUNS boots each"
bench "tvm" "$TVM" --kernel "$KERNEL" --mem "$MEM" --cmdline "$CMDLINE"

if command -v qemu-system-x86_64 >/dev/null; then
    bench "qemu microvm" qemu-system-x86_64 \
        -M microvm -enable-kvm -cpu host -m "$MEM" \
        -nodefaults -no-user-config -display none -no-reboot \
        -serial file:/dev/null \
        -kernel "$KERNEL" -append "$CMDLINE"
else
    echo "qemu-system-x86_64 not found; for a baseline: sudo apt install qemu-system-x86"
fi
