# vmm

A from-scratch virtual machine monitor in Rust. Working name; rename freely.

## Layout

- `crates/hypervisor` — backend-agnostic traits (`Hypervisor`, `Vm`, `Vcpu`, `VmExit`) and the KVM backend. A WHPX backend slots in here later.
- `crates/vmm` — the binary: guest memory, device model, vCPU run loop.

## Requirements

Linux with KVM (`/dev/kvm`), or WSL2 on Windows 11 with nested virtualization, and a stable Rust toolchain.

```sh
ls -l /dev/kvm                      # must exist
sudo usermod -aG kvm "$USER"        # then log out and back in
```

## Run

```sh
cargo run
```

Expected output (Day 1):

```
kvm api version 12
Hello from the guest!
guest halted
--- vm exits ---
io_out       22
hlt          1
total        23
```

## Progress

- [x] Day 1 — VM, guest memory, one vCPU, real-mode stub, PIO and HLT exits
- [ ] Day 2 — 64-bit long mode
- [ ] Day 3 — load a bzImage via the Linux boot protocol
- [ ] Day 4 — 8250 serial, kernel printk
- [ ] Day 5 — initrd, shell prompt
- [ ] Day 6 — exit and boot-time instrumentation, baselines
