# Architecture

hvm is a type-2 virtual machine monitor: a user-space process that asks the host kernel's hypervisor (KVM) to run guest code on the real CPU, and steps in only when the guest does something the hypervisor cannot finish on its own. This document describes how the pieces fit together.

## The two crates

```
crates/hypervisor     what a hypervisor can do, and KVM's way of doing it
crates/hvm            the machine built on top: memory, boot, devices, run loop
```

`hypervisor` defines four traits and the types they exchange:

| Trait | Role |
|---|---|
| `Hypervisor` | Creates VMs. |
| `Vm` | An address space. Maps host memory into the guest, creates the in-hypervisor interrupt controllers and timer, hands out interrupt lines and vCPUs. |
| `Vcpu` | One virtual CPU. Gets and sets registers, MSRs and CPUID, and runs the guest until it exits. |
| `IrqLine` | One interrupt line into the guest, which a device may drive from any thread. |

`Vcpu::run` returns a `VmExit`: a port read or write, an MMIO read or write, a halt, a shutdown, or an interruption by a signal. Register state crosses the boundary as plain structs (`StandardRegs`, `SpecialRegs`, `Segment`), not as KVM's own types.

Nothing in `hvm` names KVM except the one line that constructs the backend. Everything else is generic over the traits. That is what lets a second backend, such as the Windows Hypervisor Platform, be added without touching boot code or devices.

## Starting a guest

There is no firmware. A physical PC starts in 16-bit real mode and its BIOS and bootloader work their way up to 64-bit mode before the kernel runs. A hypervisor can load the end state directly, so hvm does:

1. **Memory.** Guest RAM is one anonymous host mapping, registered with the hypervisor at guest physical address 0. The host allocates pages lazily, so an idle guest costs only what it has touched.
2. **Kernel.** The protected-mode part of the bzImage is copied to 1 MiB. An initrd, if given, goes at the top of RAM.
3. **Boot parameters.** A `struct boot_params` (the "zero page") is written at 0x7000: the image's own setup header, the address of the command line, the initrd's location, and an e820 memory map.
4. **CPU state.** A flat GDT and page tables that identity-map the first 1 GiB are written into low memory. The vCPU's registers are then set to the state the Linux 64-bit boot protocol asks for: long mode on, paging on, interrupts off, RIP at the kernel's 64-bit entry point, RSI pointing at the zero page.
5. **Platform.** The hypervisor is asked for its PIC, IOAPIC, PIT and local APIC, and the boot CPU's LINT0 pin is wired to the PIC as PC firmware would leave it.

The kernel still decompresses itself, which is the largest single cost before its first log line.

`arch/x86_64` holds this: `layout.rs` (where things go), `gdt.rs`, `paging.rs`, `regs.rs`, and `linux.rs` (the boot protocol).

## The run loop

One thread owns the vCPU and does this until the guest stops:

```
loop {
    exit = vcpu.run()          // guest code runs on the real CPU
    match exit {
        port or MMIO access => platform handles it
        halt, reset         => stop
    }
}
```

Most of what a guest does never reaches this loop. Timer interrupts, the interrupt controllers, CPUID, the paravirtual clock and an idle CPU's HLT are all handled inside the hypervisor. What does reach it is I/O on devices hvm emulates.

Each trip costs a transition out of the guest, into the host kernel, out to user space and back. Keeping the number of trips down, and each one short, is most of what makes a VMM fast.

## Devices and interrupts

`devices/` holds device models as plain state machines: they take register reads and writes and say whether they want to interrupt. They know nothing about threads, the hypervisor or I/O ports, which makes them testable by replaying exactly what a guest driver does to them.

| Device | Model | Purpose |
|---|---|---|
| `serial.rs` | 16550A UART | The guest console. |
| `i8042.rs` | Keyboard controller, nothing attached | Answers the kernel's probe; resets the machine. |

`platform.rs` is the wiring. It decides which device owns which I/O port (COM1 at 0x3f8, the keyboard controller at 0x60 and 0x64), and after every access it compares the device's interrupt state with the level last driven on its line and tells the hypervisor only when that changes.

The interrupt lines are level-accurate: a line stays high for as long as the device has a reason to interrupt. The guest sees the same edges a real 8259 would.

## Threads

| Thread | Does |
|---|---|
| vCPU | Runs the guest; handles its exits. |
| console input | Reads the terminal, hands bytes to the UART, raises its interrupt. |
| console output | Collects the bytes the guest writes and puts them on the terminal a burst at a time. |

The UART is the one piece of device state shared between threads, behind a mutex that also covers its interrupt line, so the line's level always matches the device.

Input and output are separate threads because they block on different things. Output is batched because the guest writes one byte at a time, and one system call per character is measurable during boot.

## The console

While a guest runs, the terminal is in raw mode and belongs to the guest. `console.rs` puts it there, restores it on the way out, and filters a small escape sequence (Ctrl-A x) out of the input so the VMM itself can still be reached.

## Measurement

`metrics.rs` records three things on every run, at a cost of a few clock reads per exit:

- a **timeline** of milestones measured from process start, including two taken by watching the guest's console output: the first byte, and the kernel announcing its first user process;
- **exit statistics**: how many exits of each kind, how long the VMM spent on them, and how the run divides between guest and VMM;
- **I/O counts** by address, which show where exits come from.

`--bench-exits` isolates the cost of an exit itself with two guests that do nothing else. One executes CPUID, which the hypervisor answers; the other writes a port, which travels to the VMM. The gap between them is what involving user space costs on a given host.

## What is deliberately missing

- **Firmware, PCI, ACPI.** A Linux guest needs none of them to boot, and each adds devices to emulate and time to wait.
- **Emulated disks and network cards.** Storage and networking will be virtio devices, where the guest hands over whole requests through shared memory instead of poking registers one at a time.
- **A second vCPU.** It needs an MP table or ACPI to announce it, and a way to interrupt a running vCPU from another thread.
