#!/usr/bin/env bash
# Compare kernel boot time under tvm and under QEMU's microvm machine.
#
# Both boot the same kernel with the same command line and no root
# filesystem, which ends in a kernel panic. Each boot is timed from starting
# the VMM to the panic line appearing on the serial console, and the VMM is
# then stopped. Two figures are reported per boot:
#
#   wall   what a user waits: VMM start-up, firmware if any, kernel
#          decompression and the boot itself
#   guest  the kernel's own timestamp on the panic line, which starts when
#          the kernel does and so leaves out everything before it
#
# usage: scripts/boot-bench.sh [kernel]
#   RUNS=10                boots per VMM
#   MEM=512                guest memory in MiB
#   MARKER="Kernel panic"  console text that ends the measurement
#   TIMEOUT=60             seconds without console output before giving up
#
# Numbers taken inside WSL2 are nested-virtualisation numbers: every VM exit
# costs several times what it does on bare-metal Linux. Compare VMMs on the
# same host, and take absolute figures on bare metal.

set -euo pipefail

KERNEL=${1:-guest/vmlinuz-virt}
RUNS=${RUNS:-10}
MEM=${MEM:-512}
MARKER=${MARKER:-Kernel panic}
TIMEOUT=${TIMEOUT:-60}
CMDLINE="console=ttyS0 reboot=k panic=-1 pci=off"

cd "$(dirname "$0")/.."

if [[ ! -r $KERNEL ]]; then
    echo "kernel '$KERNEL' not found; see the README for how to fetch one" >&2
    exit 1
fi

echo "building tvm (release)..."
cargo build --release --quiet
TVM=target/release/vmm

# boot_once <command...>: run a VMM whose stdout is the guest console until
# MARKER appears, then stop it. Prints "<wall ms> <guest ms>"; the second is
# "-" if the marker line carries no kernel timestamp.
boot_once() {
    local start now line pid guest status=1
    start=$(date +%s%N)
    exec 3< <(exec "$@" </dev/null 2>/dev/null)
    pid=$!
    while IFS= read -r -t "$TIMEOUT" -u 3 line; do
        if [[ $line == *"$MARKER"* ]]; then
            now=$(date +%s%N)
            # "[    0.314895] Kernel panic ..." -> 314. The 10# matters: the
            # digits are "0314", which bash would otherwise read as octal.
            guest=-
            if [[ $line =~ ^\[\ *([0-9]+)\.([0-9]{3}) ]]; then
                guest=$((10#${BASH_REMATCH[1]}${BASH_REMATCH[2]}))
            fi
            echo "$(((now - start) / 1000000)) $guest"
            status=0
            break
        fi
    done
    kill "$pid" 2>/dev/null || true
    exec 3<&-
    wait "$pid" 2>/dev/null || true
    return $status
}

# median <numbers...>
median() {
    local sorted
    mapfile -t sorted < <(printf '%s\n' "$@" | sort -n)
    echo "${sorted[$(($# / 2))]}"
}

# bench <name> <command...>: boot RUNS times and summarise.
bench() {
    local name=$1 i result
    shift
    local wall=() guest=()
    for ((i = 0; i < RUNS; i++)); do
        if ! result=$(boot_once "$@"); then
            printf '%-13s boot %d never printed "%s"; run it by hand to see why:\n  %s\n' \
                "$name" "$((i + 1))" "$MARKER" "$*"
            return 0
        fi
        wall+=("${result% *}")
        guest+=("${result#* }")
    done
    local sorted
    mapfile -t sorted < <(printf '%s\n' "${wall[@]}" | sort -n)
    printf '%-13s wall: median %5d ms (min %5d, max %5d)' \
        "$name" "$(median "${wall[@]}")" "${sorted[0]}" "${sorted[$((RUNS - 1))]}"
    if [[ ${guest[0]} != - ]]; then
        printf '   guest: median %5d ms' "$(median "${guest[@]}")"
    fi
    printf '\n'
}

echo "kernel $KERNEL, $MEM MiB, $RUNS boots each, timed to \"$MARKER\""
bench "tvm" "$TVM" --kernel "$KERNEL" --mem "$MEM" --cmdline "$CMDLINE"

if command -v qemu-system-x86_64 >/dev/null; then
    bench "qemu microvm" qemu-system-x86_64 \
        -M microvm -enable-kvm -cpu host -m "$MEM" \
        -nodefaults -no-user-config -display none -no-reboot \
        -serial stdio \
        -kernel "$KERNEL" -append "$CMDLINE"
else
    echo "qemu-system-x86_64 not found; for a baseline: sudo apt install qemu-system-x86"
fi
