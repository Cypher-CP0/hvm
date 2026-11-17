# hvm

hvm is a virtual machine monitor written in Rust. It runs Linux guests on KVM with a deliberately small device model: no firmware, no legacy hardware beyond what a kernel needs to boot, and a short path between the guest and the host for everything else.

It is built around one rule: every VM exit has to be accounted for. Each run reports how many exits the guest caused, which addresses caused them and what they cost, so that a change can be judged by measurement.

## What it does

- Boots a Linux `bzImage` directly in 64-bit mode through the Linux boot protocol, with an optional initrd. There is no BIOS or bootloader stage.
- Provides a 16550A serial console with interrupts, wired to your terminal, and an i8042 controller for reset.
- Uses the hypervisor's own interrupt controllers and timer (PIC, IOAPIC, LAPIC, PIT).
- Reports a boot timeline, exit counts and timings, and the busiest I/O addresses for every run.
- Keeps the hypervisor behind a small trait interface. KVM is the only backend today; the interface is shaped so that a Windows Hypervisor Platform backend can be added without touching devices or boot code.

## Layout

- `crates/hypervisor` — backend-agnostic traits (`Hypervisor`, `Vm`, `Vcpu`, `IrqLine`, `VmExit`), x86-64 register types, and the KVM backend.
- `crates/hvm` — the binary:
  - `arch/x86_64` — memory layout, GDT, page tables, boot registers, Linux boot protocol
  - `devices` — emulated devices (16550A UART, i8042)
  - `platform.rs` — maps devices to the legacy PC I/O ports
  - `console.rs` — the host terminal
  - `metrics.rs` — exit statistics and the boot timeline
  - `guests` — assembly sources for the built-in test guests
- `scripts` — benchmarks.
- `docs/architecture.md` — how the pieces fit together.

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

Leave with `busybox reboot -f`. The guest resets and the VMM prints its report.

Without an initrd there is no root filesystem, so the boot ends in a kernel panic followed by the same reset.

## Measuring

Every run ends with three tables on stderr:

- **timeline** — when the VM was created, the kernel loaded, the vCPU ready and started, the first console byte appeared, user space started, and the guest stopped, measured from process start.
- **vm exits** — count per exit kind, and the time the VMM spent handling them. The run time is split into time inside the guest and hypervisor, and time in the VMM.
- **i/o by address** — the ports and MMIO addresses the guest touched most, and any that no device claimed.

Two benchmarks:

```sh
cargo run --release -- --bench-exits       # cost of one VM exit on this host
scripts/boot-bench.sh                      # boot time against QEMU microvm
```

`--bench-exits` runs two guests that do nothing but exit: one with an instruction the hypervisor answers itself, one with a port write that travels to the VMM and back. The difference is the price of involving user space.

`scripts/boot-bench.sh` boots the same kernel repeatedly under hvm and, if it is installed, QEMU's `microvm` machine. Each boot is timed from starting the VMM to the kernel's final panic line appearing on the console (there is no root filesystem). It prints the wall-clock time, which is what a user waits, and the kernel's own timestamp on that line, which leaves out VMM start-up, firmware and decompression.

Inside WSL2 every exit passes through a second hypervisor, which makes exits several times more expensive than on bare-metal Linux. Compare VMMs on the same host, and take absolute numbers on bare metal.

## Tests

```sh
cargo test
```

Tests that need KVM skip themselves when `/dev/kvm` is missing. Tests that boot a real kernel are opt-in:

```sh
HVM_TEST_KERNEL=$PWD/guest/vmlinuz-virt \
HVM_TEST_INITRD=$PWD/guest/initramfs-virt cargo test
```

This boots the kernel twice: once with no root filesystem, and once to a shell that is sent a command and a reboot.
