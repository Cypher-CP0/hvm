# Day 2 test guest: runs in 64-bit long mode, entered directly by the VMM.
#
# Checks a 64-bit push/pop on the boot stack and a write/read at 64 MiB
# (outside the first 2 MiB page, so it exercises the identity map), then
# prints a message on COM1 and halts.
#
# The assembled bytes are embedded in src/guest.rs. To regenerate:
#   as --64 -o hello64.o hello64.S
#   objcopy -O binary -j .text hello64.o hello64.bin
#   xxd -i hello64.bin

    .code64
    .text
    .globl _start
_start:
    mov     $0x3f8, %dx
    movabs  $0x1122334455667788, %rax
    push    %rax
    pop     %rcx
    cmp     %rax, %rcx
    jne     fail
    mov     $0x4000000, %ebx
    mov     %rax, (%rbx)
    cmp     %rax, (%rbx)
    jne     fail
    lea     msg(%rip), %rsi
    jmp     print
fail:
    lea     err(%rip), %rsi
print:
    lodsb
    test    %al, %al
    jz      done
    out     %al, %dx
    jmp     print
done:
    hlt
msg:
    .asciz  "Hello from 64-bit long mode!\n"
err:
    .asciz  "FAIL\n"
