# vmm

A from-scratch virtual machine monitor in Rust. Working name; rename freely.

## Layout

- `crates/hypervisor` — backend-agnostic traits (`Hypervisor`, `Vm`, `Vcpu`, `VmExit`), x86-64 register types, and the KVM backend. A WHPX backend slots in here later.
- `crates/vmm` — the binary: guest memory, x86-64 boot setup and Linux boot protocol (`arch/x86_64`), emulated devices (`devices/`), the legacy PC platform that maps them to I/O ports (`platform.rs`), the host terminal (`console.rs`), vCPU run loop.
- `crates/vmm/guests` — assembly sources for the built-in test guests.

## Requirements

Linux with KVM (`/dev/kvm`), or WSL2 on Windows 11 with nested virtualization, and a stable Rust toolchain.

```sh
ls -l /dev/kvm                      # must exist
sudo usermod -aG kvm "$USER"        # then log out and back in
```

## Run

```sh
cargo run                                  # built-in 64-bit test guest
cargo run -- --kernel guest/vmlinuz-virt   # boot a Linux kernel, no root filesystem
cargo run -- --kernel guest/vmlinuz-virt --initrd guest/initramfs-virt \
  --append rdinit=/bin/sh                  # boot to a shell
cargo run -- --help
cargo test                                 # KVM tests skip themselves without /dev/kvm
```

### Getting a kernel and initrd

Any x86-64 bzImage with a 64-bit entry point works (boot protocol 2.12+, which is every kernel since 3.8). Alpine's `virt` kernel is small and built for VMs, and its initramfs has a busybox shell:

```sh
mkdir -p guest
base=https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/x86_64/netboot
curl -L -o guest/vmlinuz-virt   $base/vmlinuz-virt
curl -L -o guest/initramfs-virt $base/initramfs-virt
```

`guest/` is git-ignored.

### The guest console

While a kernel runs, the terminal belongs to the guest: every key, Ctrl-C included, goes to its serial console.

- `Ctrl-A x` quits the VMM.
- `Ctrl-A Ctrl-A` sends a literal Ctrl-A to the guest.

`rdinit=/bin/sh` makes the kernel run a shell as the first process instead of the initramfs's own init script. That shell starts with only its built-in commands on the path; set up the rest with:

```sh
busybox mkdir -p /usr/bin /usr/sbin /sbin /proc /sys
busybox --install -s
mount -t proc proc /proc
```

Leave with `busybox reboot -f`. The guest resets and the VMM prints its exit statistics.

Without an initrd there is no root filesystem, so the boot ends in a kernel panic followed by the same reset. That is expected.

### Tests against a real kernel

```sh
VMM_TEST_KERNEL=$PWD/guest/vmlinuz-virt \
VMM_TEST_INITRD=$PWD/guest/initramfs-virt cargo test
```

This boots the kernel twice: once with no root filesystem, and once to a shell that is sent a command and a reboot.

## Progress

- [x] Day 1 — VM, guest memory, one vCPU, real-mode stub, PIO and HLT exits
- [x] Day 2 — 64-bit long mode: boot GDT, identity-mapped page tables, CPUID, register setup
- [x] Day 3 — load a bzImage via the Linux boot protocol; in-kernel PIC/IOAPIC/LAPIC/PIT
- [x] Day 4 — 16550A UART on COM1 with interrupts; the kernel's 8250 driver detects and uses it
- [x] Day 5 — initrd loading, console input, interactive shell
- [ ] Day 6 — exit and boot-time instrumentation, baselines
