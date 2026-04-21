# Lab 2: User Programs

---

## Information

Name: Yunuo Liu

Email: liuyunuo@stu.pku.edu.cn

> Please cite any forms of information source that you have consulted during finishing your assignment, except the TacOS documentation, course slides, and course staff.

> With any comments that may help TAs to evaluate your work better, please leave them here

The final `wait` implementation is blocking rather than busy-waiting. It stores one waiter per child process and wakes the parent explicitly when the child exits.

## Argument Passing

#### DATA STRUCTURES

> A1: Copy here the **declaration** of each new or changed struct, enum type, and global variable. State the purpose of each within 30 words.

I did not add a dedicated argument-passing-only global structure. The actual argument block is assembled with temporary local variables:

```rust
const ARG_MAX: usize = 4096;
let mut sp = exec_info.init_sp;
let mut arg_addrs: Vec<usize> = Vec::new();
```

Purpose: `ARG_MAX` bounds total argument size; `sp` tracks stack construction; `arg_addrs` records user addresses of copied strings.

#### ALGORITHMS

> A2: Briefly describe how you implemented argument parsing. How do you arrange for the elements of argv[] to be in the right order? How do you avoid overflowing the stack page?

In `SYS_EXEC`, the kernel first reads the pathname C string from user memory, then walks the user `argv[]` pointer array until it sees a null pointer. Each argument string is copied into a kernel `Vec<String>`, so argument parsing is completed before the new process is created.

In `execute()`, after loading the ELF and creating the initial user stack page, I copy argument strings to the new user stack from back to front. While doing this, I push each resulting user-space address into `arg_addrs`. Because the strings are processed in reverse order, the addresses end up in forward argument order inside `arg_addrs`. I then push `argv[argc] = NULL` and iterate through `arg_addrs` in its stored order, so the final `argv[]` seen by user space is `argv[0], argv[1], ..., argv[argc - 1], NULL`.

To avoid overflowing the single initial stack page, I compute the total required space before writing anything: sum of all strings including terminators, plus the pointer array including the final null pointer, plus some alignment slack. If the total exceeds `ARG_MAX = 4096`, `execute()` destroys the partially prepared page table and returns `-1`.

#### RATIONALE

> A3: In Tacos, the kernel reads the executable name and arguments from the command. In Unix-like systems, the shell does this work. Identify at least two advantages of the Unix approach.

One advantage is modularity: the kernel only executes programs, while parsing, quoting, wildcard expansion, pipes, and redirections stay in user space. That keeps the kernel smaller and simpler.

Another advantage is flexibility: different shells can provide different command languages and user experiences without changing the kernel ABI at all.

A third advantage is safety and maintainability: string parsing bugs stay in ordinary user programs instead of privileged kernel code.

## System Calls

#### DATA STRUCTURES

> B1: Copy here the **declaration** of each new or changed struct, enum type, and global variable. State the purpose of each within 30 words.

```rust
pub struct UserProc {
    #[allow(dead_code)]
    bin: Mutex<Option<File>>,
    pub fd_table: Mutex<FdTable>,
}
```

Purpose: per-process state. `bin` keeps the executable open and denied for writes while running; `fd_table` stores the process-local file descriptor table.

```rust
pub struct ProcInfo {
    parent_tid: AtomicIsize,
    state: SyncMutex<WaitState, Spin>,
}
```

Purpose: global per-process wait metadata stored in `proc_table`, used for parent-child tracking and `wait()` synchronization.

```rust
struct WaitState {
    has_exited: bool,
    exit_status: isize,
    waiter: Option<Arc<thread::Thread>>,
}
```

Purpose: child exit state plus the blocked parent thread, if any.

```rust
pub enum FileType {
    File(File),
    Stdin,
    Stdout,
    Stderr,
}
```

Purpose: distinguishes ordinary files from standard I/O objects in the descriptor table.

```rust
pub struct OpenFile {
    pub file: FileType,
    pub readable: bool,
    pub writable: bool,
}
```

Purpose: descriptor entry combining an opened object with its access mode.

```rust
pub struct FdTable {
    next_fd: usize,
    table: BTreeMap<usize, OpenFile>,
}
```

Purpose: per-process map from file descriptor integers to `OpenFile` records.

Open flag constants:

```rust
pub const O_RDONLY: usize = 0x000;
pub const O_WRONLY: usize = 0x001;
pub const O_RDWR: usize = 0x002;
pub const O_CREATE: usize = 0x200;
pub const O_TRUNC: usize = 0x400;
```

Purpose: encode access mode and creation/truncation behavior for `open`.

> B2: Describe how file descriptors are associated with open files. Are file descriptors unique within the entire OS or just within a single process?

Each user process owns one `FdTable`. The table is a `BTreeMap<usize, OpenFile>`, and `OpenFile` stores both the underlying object (`File`, `Stdin`, `Stdout`, `Stderr`) and the descriptor’s readable/writable permission bits.

Descriptors are unique only within a single process. Different processes can both have descriptor `3`, but those integers refer to entries in different `FdTable`s.

#### ALGORITHMS

> B3: Describe your code for reading and writing user data from the kernel.

I use two layers.

The first layer is a coarse validation helper, `is_valid_area(ptr, len)`, used by many syscalls before touching a user buffer. It checks for overflow in `ptr + len`, rejects invalid mappings, and rejects kernel addresses.

The second layer is in `src/mem/userbuf.rs`. `read_user_byte()` and `write_user_byte()` use small assembly stubs to touch user memory one byte at a time. If the access faults, the page-fault handler redirects execution to the helper’s recovery label and returns `Err(BadPtr)`. On top of those byte primitives, I built `read_user_cstr`, `read_user_buf`, `read_user_usize`, `write_user_buf`, and `write_user_usize`.

So a typical syscall either reads a user C string or copies a whole user buffer by repeatedly using those helpers, while ordinary Rust `Result` handling keeps the call sites readable.

> B4: Suppose a system call causes a full page (4,096 bytes) of data to be copied from user space into the kernel. 
> What is the least and the greatest possible number of inspections of the page table 
> (e.g. calls to `Pagetable.get_pte(addr)` or other helper functions) that might result?
> What about for a system call that only copies 2 bytes of data?
> Is there room for improvement in these numbers, and how much?

For the current implementation of syscalls such as `write`, the software page-table inspections happen in `is_valid_area(ptr, len)`.

For a 4,096-byte copy, the least and greatest numbers are both 2. The helper checks the first address in the range and also checks `end - 1`. Because the length is exactly 4,096, the loop body runs once and the tail check runs once.

For a 2-byte copy, the least and greatest numbers are also both 2 for the same reason: one check at the start address and one check at `end - 1`.

There is room for improvement. If the start and end addresses fall in the same page, the code could avoid re-checking that page, reducing those cases from 2 inspections to 1. More generally, the code could validate page-by-page and skip duplicate checks on already covered pages.

> B5: Briefly describe your implementation of the "wait" system call and how it interacts with process termination.

Each user process has an entry in the global `proc_table`, keyed by its thread id. The entry stores the parent tid and a `WaitState` protected by a spin-based mutex.

When a parent calls `wait(pid)`, the kernel first checks that `pid` exists and that its recorded `parent_tid` matches the caller. If not, `wait` returns `None`, which the syscall layer converts to `-1`.

If the child has already exited, `wait` reads the saved exit code, removes the child’s `ProcInfo` from `proc_table`, and returns the code immediately.

Otherwise, `wait` stores the current parent thread in `state.waiter`, marks the parent as `Blocked`, and calls the scheduler. When the child later calls `exit(status)`, it saves the exit code, marks `has_exited = true`, takes the waiting parent out of `state.waiter`, and wakes that thread with `thread::wake_up()`.

The child’s executable file handle is dropped before the parent can observe completion, so `rox-*` tests see the executable become writable again as soon as `wait()` returns.

> B6: Any access to user program memory at a user-specified address
> can fail due to a bad pointer value.  Such accesses must cause the
> process to be terminated.  System calls are fraught with such
> accesses, e.g. a "write" system call requires reading the system
> call number from the user stack, then each of the call's three
> arguments, then an arbitrary amount of user memory, and any of
> these can fail at any point.  This poses a design and
> error-handling problem: how do you best avoid obscuring the primary
> function of code in a morass of error-handling?  Furthermore, when
> an error is detected, how do you ensure that all temporarily
> allocated resources (locks, buffers, etc.) are freed?
> Have you used some features in Rust, to make these things easier than in C?
> In a few paragraphs, describe the strategy or strategies you adopted for
> managing these issues.  Give an example.

I tried to separate user-memory fault handling from syscall business logic. Instead of open-coding pointer chasing everywhere, I centralized user accesses in `userbuf.rs`. Syscalls mostly say “read this C string”, “read this usize”, or “copy this buffer”, and then use ordinary `match` / `Result` flow.

I also separate two kinds of failures. First, many syscalls do a cheap pre-check with `is_valid_area`; if that fails, the syscall returns `-1`. Second, even after validation, the actual bytewise helper may still fault, and then the assembly helper plus page-fault handler turn that into `Err(BadPtr)`. User-mode faults outside these controlled helpers still terminate the process with `exit(-1)`.

Rust makes cleanup much simpler than C here. Temporary kernel buffers are ordinary `Vec<u8>`, so they free automatically when returning. Locks use RAII guards, so even an early `return -1` releases them correctly. For example, in `SYS_READ`, if the descriptor lookup succeeds but `write_user_buf(ptr, &kbuf[..n])` fails, the function simply returns `-1`; the file-table lock guard and the temporary kernel buffer are both dropped automatically.

> B7: Briefly describe what will happen if loading the new executable fails. (e.g. the file does not exist, is in the wrong format, or some other error.)

If the pathname does not exist, `DISKFS.open()` in `SYS_EXEC` fails and the syscall returns `-1`.

If the file exists but is not a valid ELF executable, `load_executable()` returns an error. `execute()` then destroys the partially created page table and returns `-1`.

If argument copying into the new user stack fails, `execute()` restores the old page table, destroys the new one, and returns `-1`.

In all of these cases, no child thread is registered in `proc_table`, so there is no zombie or half-created process left behind.

#### SYNCHRONIZATION

> B8: Consider parent process P with child process C.  How do you
> ensure proper synchronization and avoid race conditions when P
> calls wait(C) before C exits?  After C exits?  How do you ensure
> that all resources are freed in each case?  How about when P
> terminates without waiting, before C exits?  After C exits?  Are
> there any special cases?

If `P` calls `wait(C)` before `C` exits, `P` records itself in `C`’s `WaitState.waiter`, marks itself blocked, and yields the CPU. When `C` exits, it writes its exit status, wakes `P`, and `P` reaps `C` by removing its `ProcInfo` entry from `proc_table`.

If `P` calls `wait(C)` after `C` has already exited, `C` is a zombie: its thread is gone, but its `ProcInfo` remains in `proc_table` with `has_exited = true`. `wait` then returns the saved exit code immediately and removes the zombie entry.

If `P` terminates without waiting and `C` is still alive, `orphan_children()` changes `C.parent_tid` to `NO_PARENT`. Later, when `C` exits, it sees there is no parent and removes its own `ProcInfo` immediately. This avoids a leaked zombie.

If `P` terminates after `C` has already exited but before calling `wait`, `orphan_children()` finds that `C` is already a zombie and removes that `ProcInfo` on the parent’s behalf.

The main special cases are that `wait` only succeeds for a direct child, and that a child can be reaped only once. After a successful `wait`, the child's `ProcInfo` is removed from `proc_table`, so any later `wait` on the same pid fails immediately.

#### RATIONALE

> B9: Why did you choose to implement access to user memory from the
> kernel in the way that you did?

I chose this design because it is simple and localizes risk. The page-fault-aware byte helpers hide the ugly recovery mechanism, while the higher-level wrappers expose a small safe API to the rest of the kernel.

This approach is not the fastest, because whole buffers are copied byte by byte, but it keeps the syscall code readable and robust enough for Lab 2.

> B10: What advantages or disadvantages can you see to your design
> for file descriptors?

Advantages:

1. The design is simple: each process has one table, standard descriptors are preinstalled, and descriptor lookup is straightforward.
2. Read/write permission is checked explicitly in `OpenFile`, so invalid accesses are easy to reject.
3. Per-process descriptor numbering matches Unix intuition and keeps user-visible behavior simple.

Disadvantages:

1. `next_fd` only grows, so closed descriptors are not reused.
2. There is no shared open-file layer between processes, so descriptor inheritance/sharing semantics are limited.
3. `BTreeMap` is simple but not the most space- or time-efficient possible structure.

> B11: What is your tid_t to pid_t mapping. What advantages or disadvantages can you see to your design?

My design uses the kernel thread id directly as the user-visible process id: `pid_t == tid_t` for user processes.

The main advantage is simplicity. The scheduler already gives every thread a unique id, so `exec` can return that id directly and `wait` can use it as the lookup key in `proc_table`.

The downside is that the abstraction boundary is weak: process identity is tied directly to the kernel thread implementation. If the kernel later supports multiple threads per process, or separate process objects, this mapping would need redesign.
