# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Tacos is Pintos reimplemented in Rust for riscv64 — the skeleton for Peking University's undergraduate OS course (honor track). It is a RISC-V 64-bit kernel that runs on QEMU with OpenSBI firmware. Upstream repo: [PKU-OS/Tacos](https://github.com/PKU-OS/Tacos).

## Build & Run

```bash
make              # Build user programs (C), mkfs, disk.img
make run          # Build everything + run kernel in QEMU with tests
make run-gdb      # Run with GDB server attached (-s -S)
make format       # cargo fmt + clang-format on C files
make clean        # Remove build/ + cargo clean (kernel and tool)
make clean-tacos  # Clean only kernel artifacts
```

The kernel binary is at `target/riscv64gc-unknown-none-elf/debug/tacos` (or `release/`). The runner script is `./tacos`, which copies `build/disk.img` and launches QEMU.

The companion CLI tool lives in `tool/`:
```bash
cargo run -p tool -- build                # wraps make
cargo run -p tool -- test -c <case>       # run specific test
cargo run -p tool -- test -b lab1         # run all Lab 1 tests
cargo run -p tool -- test --gdb -c <case> # run single test with GDB
cargo run -p tool -- test --grade         # graded output
cargo run -p tool -- test --verbose       # full output, no timeout suppression
```

## Testing

All tests run inside QEMU. The makefile provides shorthand:

```bash
make test-<name>          # Kernel-level test (e.g., make test-sync)
make test-user-<name>     # User program test (e.g., make test-user-args-none)
make test-schedule-<name> # Equivalent: make test-<name>
make gdb-<name>           # Run a test with GDB attached
```

Tests are selected via Cargo features. Key feature flags in `Cargo.toml`:
- `test` — base test feature (runs test dispatcher instead of shell)
- `test-unit`, `test-schedule`, `test-user` — test mode selectors
- `test-sync`, `test-thread-adder`, `test-fs-disk`, `test-alarm-single`, `test-donation-chain`, etc. — individual test cases
- `thread-scheduler-priority` — enables priority-based scheduling (required for priority/donation tests)
- `shell` — interactive kernel monitor
- `debug` — enable debug kprintln output

Test sources are in `test/`. `test/mod.rs` is the dispatcher; it conditionally runs unit, schedule, or user tests based on enabled features.

Test catalogs with grade points and timeouts: `tool/bookmarks/unit.toml`, `lab1.toml`, `lab2.toml`, `lab3.toml`.

## Course structure

The course has four labs, each building on the last. Full documentation is at [pku-tacos.pages.dev](https://pku-tacos.pages.dev/introduction).

- **Lab 0 (Appetizer):** Booting, debugging, and building a kernel monitor (shell). Covers the HelloWorld and Memory chapters of the docs.
- **Lab 1 (Scheduling):** Thread sleeping, priority scheduling, and priority donation. Corresponds to the Thread chapter.
- **Lab 2 (User Programs):** System calls — process control (`wait`, `exit`, `exec`) and file I/O (`read`, `write`, `open`, etc.).
- **Lab 3 (Virtual Memory):** Page swapping and the `mmap` system call.

The kernel is modular. Some subsystems (`sbi`, memory allocators, trap handling) are always required and stay unchanged across labs. Others (userproc, syscall, pagefault) are added or extended per lab.

## Architecture

### Boot sequence

1. **OpenSBI** loads the kernel at `0x80200000` (physical) and jumps to `_entry` in `src/boot.rs`
2. `_entry` sets up the Sv39 page table mapping `[0xFFFFFFC080000000..)` → `[0x80000000..)`, enables MMU, jumps to high virtual address, and calls `main()`
3. `main()` in `src/main.rs` initializes: BSS clear → device tree parse → memory (palloc, kalloc, user pool) → trap init → PLIC → virtio → timer → either runs tests or the kernel shell

### Memory layout (important constants in `src/mem/layout.rs`)

- Physical memory: starts at `0x8000_0000` (128 MiB)
- Kernel virtual base: `0xFFFFFFC0_8020_0000` (Sv39 page-based, 2 MiB aligned)
- User stack: starts at `0x8000_0000_0000`, grows down
- Trampoline page: highest user page, shared kernel/user for trap entry/exit
- Physical page size: 4 KiB

### Thread subsystem (`src/thread/`)

- `imp.rs` — Thread struct, Builder pattern, Status enum (Ready/Running/Blocked/Dying)
- `manager.rs` — Thread Manager singleton: spawns threads, manages sleep/wake queues, owns the process table. A new thread first runs in a "nursery" for initialization before being enqueued to the scheduler.
- `scheduler.rs` — Scheduler trait; `fcfs.rs` (default FIFO), `priority.rs` (priority with donation)
- `switch.rs` — Context switch in assembly (`global_asm!`)

### Synchronization (`src/sync/`)

Standard primitives built on RISC-V atomic operations and interrupt disabling: SpinLock, Mutex (blocking), Semaphore, Condition Variable, SleepLock, Once, Lazy.

### File system (`src/fs/`)

- `disk.rs` — `DiskFs`: manages a raw block device (virtio) with free_map, inodes, swap, root directory. Global `DISKFS` singleton.
- `inmem.rs` — `MemFs`: in-memory filesystem for kernel-internal files.
- `io.rs` — `Read`/`Write`/`Seek` traits implemented by both FS types.

### Trap handling (`src/trap/`)

- Trap entry assembly in `src/trap.rs` (`global_asm!`) — saves/restores all registers, routes to Rust handlers
- `syscall.rs` — 12 system calls: halt, exit, exec, wait, open, read, write, create, delete, seek, filesize, mmap
- `pagefault.rs` — handles user page faults (lazy allocation, copy-on-write, mmap)

### User processes (`src/userproc/`)

- `load.rs` — Parses ELF headers, allocates page table + user pages, loads segments
- `fdtable.rs` — Per-process file descriptor table

### Disk image (`mkfs.c`)

10 MiB disk image with a custom layout: boot sector → free_map (1 sector) → root directory (1 sector) → inodes → data blocks → swap area. Created by `mkfs.c` (compiled with host GCC) during `make`.

### User programs (`user/`)

C programs cross-compiled with `riscv64-unknown-elf-gcc`. The user library in `user/lib/` provides syscall stubs (generated by `usys.pl`), a minimal libc, and a linker script. User ELFs are bundled into `build/disk.img` and loaded by the kernel at runtime.

## Cross-compilation toolchain

- Target: `riscv64gc-unknown-none-elf`
- GCC: `riscv64-unknown-elf-gcc` (for user programs)
- Linker script: `src/linker.ld`
- SBI firmware: `fw_jump.bin` (OpenSBI)
- Docker image: `crimmypeng/tacos:rust-1.92v3` (see `Dockerfile` and `.devcontainer/`)
