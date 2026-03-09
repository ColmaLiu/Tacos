# Lab 1: Appetizer

---

## Information

Name: Yunuo Liu

Email: liuyunuo@stu.pku.edu.cn

> Please cite any forms of information source that you have consulted during finishing your assignment, except the TacOS documentation, course slides, and course staff.

> With any comments that may help TAs to evaluate your work better, please leave them here

## Booting Tacos

> A1: Put the screenshot of Tacos running example here.

![booting](booting.png)

## Debugging

### First instruction

> B1: What is the first instruction that gets executed?

auipc   t0,0x0

> B2: At which physical address is this instruction located?

0x1000

### From ZSBL to SBI

> B3: Which address will the ZSBL jump to?

0x80000000

### SBI, kernel and argument passing

> B4: What's the value of the argument `hard_id` and `dtb`?

```
(gdb) b main
Breakpoint 3 at 0xffffffc080203ae2: file src/main.rs, line 51.
(gdb) c
Continuing.
...
Breakpoint 3, tacos::main (hart_id=0, dtb=2183135232) at src/main.rs:51
51          kprintln!("Hello, World!");
```

0, 2183135232=0x82200000

> B5: What's the value of `Domain0 Next Address`, `Domain0 Next Arg1`, `Domain0 Next Mode` and `Boot HART ID` in OpenSBI's output?

0x0000000080200000, 0x0000000082200000, S-mode, 0

> B6: What's the relationship between the four output values and the two arguments?

`Domain0 Next Address` is the entry point address where OpenSBI will jump to after initialization.

`Domain0 Next Arg1` is the first argument passed to the OS kernel when OpenSBI jumps to Next Address, which is equal to `dtb`.

`Domain0 Next Mode` is the privilege mode that the OS kernel will run in.

`Boot HART ID` is the Hardware Thread (HART) ID of the core that performed the initial boot, which is equal to `hard_id`.

### SBI interfaces

> B7: Inside `console_putchar`, Tacos uses `ecall` instruction to transfer control to SBI. What's the value of register `a6` and `a7` when executing that `ecall`?

```
(gdb) b console_putchar
Breakpoint 4 at 0xffffffc0802187de: file src/sbi.rs, line 55.
(gdb) c
Continuing.

Breakpoint 4, tacos::sbi::legacy::console_putchar (char=91) at src/sbi.rs:55
55              call!(CONSOLE_PUTCHAR; char);
(gdb) x/5i $pc
=> 0xffffffc0802187de <_ZN5tacos3sbi6legacy15console_putchar17h83c085bac921d336E+16>:   ecall
   0xffffffc0802187e2 <_ZN5tacos3sbi6legacy15console_putchar17h83c085bac921d336E+20>:   sd      a0,-40(s0)
   0xffffffc0802187e6 <_ZN5tacos3sbi6legacy15console_putchar17h83c085bac921d336E+24>:   sd      a1,-32(s0)
   0xffffffc0802187ea <_ZN5tacos3sbi6legacy15console_putchar17h83c085bac921d336E+28>:   ld      ra,40(sp)
   0xffffffc0802187ec <_ZN5tacos3sbi6legacy15console_putchar17h83c085bac921d336E+30>:   ld      s0,32(sp)
(gdb) p/x $a6
$2 = 0x0
(gdb) p/x $a7
$3 = 0x1
```

0, 1

## Kernel Monitor

> C1: Put the screenshot of your kernel monitor running example here. (It should show how your kernel shell respond to `whoami`, `exit`, and `other input`.)

![monitor](monitor.png)

> C2: Explain how you read and write to the console for the kernel monitor.

In the kernel monitor loop , I read input one byte at a time by calling
`sbi::console_getchar()`. This function uses the SBI legacy console-getchar service
(`EID = 0x02`) and enters machine mode through `ecall`.
The monitor stores bytes into a local buffer, handles backspace (`0x08`/`0x7f`), and
stops reading when it sees `\r` or `\n`. Then it converts the collected bytes to UTF-8
and matches commands such as `whoami` and `exit`.

For output, the monitor uses `kprint!`/`kprintln!`. These macros write formatted strings
to `sbi::console::Stdout`, whose `write_str()` sends each character to
`sbi::console_putchar()`. `console_putchar()` invokes the SBI legacy putchar service
(`EID = 0x01`) through `ecall`, so characters are finally shown on the QEMU/OpenSBI
serial console.
