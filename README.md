# vmm

A from-scratch virtual machine monitor in Rust. Working name; rename freely.

## Layout

- `crates/hypervisor` — backend-agnostic traits (`Hypervisor`, `Vm`, `Vcpu`, `VmExit`), x86-64 register types, and the KVM backend. A WHPX backend slots in here later.
- `crates/vmm` — the binary: guest memory, x86-64 boot setup and Linux boot protocol (`arch/x86_64`), legacy port I/O stub (`platform.rs`), vCPU run loop.
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
cargo run -- --kernel guest/vmlinuz-virt   # boot a Linux kernel
cargo run -- --help
cargo test                                 # KVM tests skip themselves without /dev/kvm
```

### Getting a kernel

Any x86-64 bzImage with a 64-bit entry point works (boot protocol 2.12+, which is every kernel since 3.8). Alpine's `virt` kernel is small and built for VMs:

```sh
mkdir -p guest
curl -L -o guest/vmlinuz-virt \
  https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/x86_64/netboot/vmlinuz-virt
```

`guest/` is git-ignored.

### What to expect (Day 3)

There is no root filesystem yet, so a successful boot is the full kernel log ending in a panic, after which the guest resets and the VMM exits:

```
kvm api version 12
kernel loaded at 0x100000..0x..., entry 0x100200
[    0.000000] Linux version ...
...
[    ...] Kernel panic - not syncing: VFS: Unable to mount root fs on unknown-block(0,0)
guest requested reset
--- vm exits ---
...
```

To include the kernel boot in the test suite: `VMM_TEST_KERNEL=$PWD/guest/vmlinuz-virt cargo test`.

If the log stops right after a `console [ttyS0] enabled` line, add `keep` to the early console, which keeps it printing for the whole boot:

```sh
cargo run -- --kernel guest/vmlinuz-virt --cmdline \
  "earlyprintk=serial,ttyS0,115200,keep nokaslr reboot=k panic=-1 pci=off"
```

## Progress

- [x] Day 1 — VM, guest memory, one vCPU, real-mode stub, PIO and HLT exits
- [x] Day 2 — 64-bit long mode: boot GDT, identity-mapped page tables, CPUID, register setup
- [x] Day 3 — load a bzImage via the Linux boot protocol; in-kernel PIC/IOAPIC/LAPIC/PIT
- [ ] Day 4 — 8250 serial, kernel printk
- [ ] Day 5 — initrd, shell prompt
- [ ] Day 6 — exit and boot-time instrumentation, baselines
