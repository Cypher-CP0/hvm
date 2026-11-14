# vmm

A from-scratch virtual machine monitor in Rust. Working name; rename freely.

## Layout

- `crates/hypervisor` — backend-agnostic traits (`Hypervisor`, `Vm`, `Vcpu`, `VmExit`), x86-64 register types, and the KVM backend. A WHPX backend slots in here later.
- `crates/vmm` — the binary: guest memory, x86-64 boot setup (`arch/x86_64`), vCPU run loop.
- `crates/vmm/guests` — assembly sources for the built-in test guests.

## Requirements

Linux with KVM (`/dev/kvm`), or WSL2 on Windows 11 with nested virtualization, and a stable Rust toolchain.

```sh
ls -l /dev/kvm                      # must exist
sudo usermod -aG kvm "$USER"        # then log out and back in
```

## Run

```sh
cargo run
cargo test      # the end-to-end test skips itself without /dev/kvm
```

Expected output (Day 2):

```
kvm api version 12
Hello from 64-bit long mode!
guest halted
--- vm exits ---
io_out       29
hlt          1
total        30
```

## Progress

- [x] Day 1 — VM, guest memory, one vCPU, real-mode stub, PIO and HLT exits
- [x] Day 2 — 64-bit long mode: boot GDT, identity-mapped page tables, CPUID, register setup
- [ ] Day 3 — load a bzImage via the Linux boot protocol
- [ ] Day 4 — 8250 serial, kernel printk
- [ ] Day 5 — initrd, shell prompt
- [ ] Day 6 — exit and boot-time instrumentation, baselines
