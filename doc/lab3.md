# Lab 3: Virtual Memory

---

## Information

Name: Yunuo Liu

Email: liuyunuo@stu.pku.edu.cn

> Please cite any forms of information source that you have consulted during finishing your assignment, except the TacOS documentation, course slides, and course staff.

> With any comments that may help TAs to evaluate your work better, please leave them here

The eviction strategy is same-process-only: each process evicts its own frames. This avoids cross-process locking complexity on a single-CPU kernel. To compensate, the UserPool is expanded to 512 pages and the parent pre-evicts 16 pages before spawning a child, giving the child a working set without contention. The swap slot allocator uses a free stack for O(1) alloc/free instead of a bitmap scan.

## Stack Growth

#### ALGORITHMS

> A1: Explain your heuristic for deciding whether a page fault for an invalid virtual address should cause the stack to be extended into the page that faulted.

When a user-mode page fault occurs and the address is not found in the supplementary page table, the handler checks three conditions before growing the stack:

1. **Fault type**: Only `LoadPageFault` and `StorePageFault` can trigger stack growth. `InstructionPageFault` never grows the stack.

2. **Fault page matches sp**: The faulting page must equal the user's stack pointer page (`frame.x[2] & !PG_MASK`). This distinguishes genuine stack accesses from wild pointer dereferences — if the program's `sp` is in the faulting page, the access is plausibly a stack operation.

3. **Address within stack region**: The faulting page must lie in `[USER_STACK_TOP - MAX_STACK_SIZE, USER_STACK_TOP)`, i.e., within 8 MB below `0x80500000`, and above `0x800` (guarding against null-pointer-like stack underflow).

The implementation is in `grow_stack()` ([src/trap/pagefault.rs](src/trap/pagefault.rs#L395-L448)). Rather than growing one page at a time, it bulk-grows: starting from the faulting page, it allocates pages upward until it hits an already-mapped page (the existing stack). Each page is zero-filled, mapped with `R|W|U|V`, recorded as `AnonFrame` in the supplementary page table, and registered in the global frame table for eviction tracking.

Stack growth can also be triggered from kernel mode — when a syscall (e.g., `read`) writes into a stack buffer whose page hasn't been touched yet, the `__knrl_write_usr_byte` helper faults in supervisor mode. The handler (`handle_kernel_user_fault`) checks the same stack region bounds and calls `grow_stack()` if the faulting page is unmapped and within the valid stack range ([src/trap/pagefault.rs](src/trap/pagefault.rs#L331-L336)).

If the fault address is below `USER_STACK_TOP - MAX_STACK_SIZE` (i.e., stack would exceed 8 MB), growth is denied and the process is killed with exit code -1. This implements the `pt-grow-bad` test case.

## Memory Mapped Files

#### DATA STRUCTURES

> B1: Copy here the declaration of each new or changed struct or struct member, global or static variable, typedef, or enumeration. Identify the purpose of each in 25 words or less.

```rust
pub struct MmapTable {
    regions: Vec<MmapRegion>,
    next_id: usize,
}
```

Per-process table of active mmap regions. `next_id` is a monotonically increasing counter for generating unique mapid values starting from 1.

```rust
pub struct MmapRegion {
    pub mapid: usize,
    pub file: File,
    pub addr: usize,
    pub size: usize,
    pub pages: usize,
    pub writable: bool,
}
```

Describes one mmap region: the backing file, user virtual address, byte size, page count, and whether writes are allowed.

```rust
pub enum PageSource {
    // ... existing variants ...
    MmapFile {
        file: File,
        offset: usize,
        mapid: usize,
        writable: bool,
    },
    MmapSwap {
        swap_index: usize,
        mapid: usize,
        flags: PTEFlags,
    },
    AnonSwap { swap_index: usize, flags: PTEFlags },
    AnonFrame { phys_addr: usize, flags: PTEFlags },
}
```

New `PageSource` variants in the supplementary page table. `MmapFile` tags pages demand-loaded from a mapped file. `MmapSwap` tracks mmap pages evicted to swap. `AnonSwap` and `AnonFrame` track anonymous (stack) pages.

```rust
pub struct UserProc {
    // ... existing fields ...
    pub supp_page: Mutex<SuppPageTable>,
    pub mmap_table: Mutex<MmapTable>,
}
```

Per-process supplementary page table and mmap region tracker, both protected by blocking mutexes.

New syscall constants ([src/trap/syscall.rs](src/trap/syscall.rs#L37-L38)):
```rust
const SYS_MMAP: usize = 13;
const SYS_MUNMAP: usize = 14;
```

#### ALGORITHMS

> B2: Describe how memory mapped files integrate into your virtual memory subsystem. Explain how the page fault and eviction processes differ between swap pages and other pages.

**Integration**: When `mmap` is called, the kernel inserts `PageSource::MmapFile` entries into the supplementary page table for every page of the mapping. No physical pages are allocated at mmap time — all pages are demand-loaded. The mmap region is also recorded in `MmapTable` for overlap detection and for cleanup on `munmap`/`exit`.

**Page fault for mmap pages**: When a page fault hits an address whose supp entry is `MmapFile`, the handler treats it like an `InFile` load: allocates a physical frame, seeks the backing file to the correct offset, reads one page of data, zero-fills any tail beyond file length, and maps the PTE with `R|U|V` (plus `W` if the region is writable, but never `X`). The handler does NOT mutate the supp entry to `InFrame` — it stays as `MmapFile` so that future evictions know to write back to the original file. The PTE's V bit alone tells the fault handler that the page is already present.

**Eviction for mmap pages**: When a dirty `MmapFile` frame is evicted, the data is written back to the source file at the correct offset (`EvictAction::WriteFile`). The supp entry remains `MmapFile` — on next access, the page is re-read from the file. When a clean `MmapFile` frame is evicted, the frame is simply discarded with no I/O. If writeback fails (e.g., the file is on a read-only medium), the dirty page is instead written to swap and the supp entry becomes `MmapSwap`.

**Difference from regular file-backed pages** (`InFile`): Regular code/data pages are never written back to the executable file. If clean, they are silently discarded on eviction; if dirty, they go to swap (`InSwap`). This preserves the invariant that executables are not modified at runtime. Mmap pages, in contrast, must write dirty data back to the mapped file.

**Swap pages** (`InSwap`, `AnonSwap`, `MmapSwap`): On fault, the handler calls `swap_in()`, which allocates a frame, reads from the swap slot via `SwapManager::read_slot`, frees the swap slot, maps the PTE, and updates the supp entry to `InFrame`/`AnonFrame`/`MmapFile` respectively.

> B3: Explain how you determine whether a new file mapping overlaps any existing segment.

`MmapTable::insert()` ([src/userproc.rs](src/userproc.rs#L58-L106)) checks three kinds of overlap:

1. **Existing mmap regions**: Iterates through `self.regions` and checks whether the new mapping's address range `[addr, addr + pages * PG_SIZE)` intersects any existing region's range.

2. **Loaded segments (code/data/BSS)**: Iterates through the supplementary page table. Since the loader inserts an `InFile` entry for every page of every LOAD segment, any existing code or data page will appear in the supp table. The check uses the same interval-overlap test.

3. **Stack region**: Computes `[USER_STACK_TOP - MAX_STACK_SIZE, USER_STACK_TOP)` and checks overlap with the new mapping.

The overlap test itself is a standard open-interval check:
```rust
let overlaps = |other_start, other_end| addr < other_end && new_end > other_start;
```

Additionally, `SYS_MMAP` pre-validates that `addr != 0`, `addr` is page-aligned, `fd > 2`, and `file_len > 0` before calling `insert()`.

#### RATIONALE

> B4: Mappings created with "mmap" have similar semantics to those of data demand-paged from executables, except that "mmap" mappings are written back to their original files, not to swap. This implies that much of their implementation can be shared. Explain why your implementation either does or does not share much of the code for the two situations.

My implementation shares a great deal of code between mmap pages and executable data pages.

On the **fault path**, both `MmapFile` and `InFile` use the same `FaultAction::LoadFromFile` variant and the same `load_from_file()` function. The only difference is the PTE flags: `InFile` pages inherit the ELF segment flags (which may include `X`), while `MmapFile` pages use `R|U|V` plus optionally `W`, never `X`.

On the **eviction path**, the behavior diverges intentionally. `InFile` pages that are clean are silently discarded (the supp entry stays `InFile` — re-read from executable on next fault). `InFile` pages that are dirty go to swap. `MmapFile` pages that are dirty are written back to the mapped file. This divergence is handled by `evict_update_supp()` returning different `EvictAction` values based on the `PageSource` variant, while the surrounding eviction machinery (clock scan, page copy, PTE unmap, frame free) is fully shared.

On the **cleanup path** (`munmap`/`exit`), mmap pages get special handling via `collect_dirty_writebacks()` and `cleanup_mapid()`, which iterate supp entries filtered by `mapid`. Regular pages are cleaned up by the generic "iterate all supp entries, free frames, unmap PTEs" loop.

The shared infrastructure (supplementary page table, frame table, page fault dispatch) made this separation natural — about 80% of the code path is shared.

## Page Table Management

#### DATA STRUCTURES

> C1: Copy here the **declaration** of each new or changed struct, enum type, and global variable. State the purpose of each within 30 words.

```rust
pub struct SuppPageTable {
    entries: BTreeMap<usize, SuppEntry>,
}
```

Per-process map from page-aligned virtual address to backing-store metadata. Replaces the old approach of eagerly allocating pages for every LOAD segment.

```rust
pub struct SuppEntry {
    pub source: PageSource,
}
```

One entry in the supplementary page table, holding the backing-store descriptor for a single virtual page.

```rust
pub enum PageSource {
    InFrame { phys_addr: usize, flags: PTEFlags },
    InSwap { swap_index: usize, flags: PTEFlags },
    InFile { file: File, offset: usize, filesz: usize, flags: PTEFlags },
    MmapFile { file: File, offset: usize, mapid: usize, writable: bool },
    MmapSwap { swap_index: usize, mapid: usize, flags: PTEFlags },
    AnonSwap { swap_index: usize, flags: PTEFlags },
    AnonFrame { phys_addr: usize, flags: PTEFlags },
}
```

Discriminated union for where a page's data lives: in a physical frame, in a swap slot, in the executable file, in an mmap'd file, or evicted from any of those to swap. Each variant carries the flags needed to restore the PTE.

```rust
pub struct FrameTable {
    frames: BTreeMap<usize, FrameEntry>,
    clock_hand: usize,
    keys: alloc::vec::Vec<usize>,
    keys_dirty: bool,
}
```

Global singleton mapping physical frame address to metadata. `clock_hand` is the clock algorithm cursor; `keys` is a cached vector of frame addresses for O(1) indexed scanning; `keys_dirty` tracks when the cache needs refresh.

```rust
pub struct FrameEntry {
    pub owner_tid: isize,
    pub user_va: usize,
    pub ref_bit: bool,
    pub pinned: bool,
}
```

Per-frame metadata: owning thread id, user virtual address backed by this frame, clock reference bit, and pin flag that prevents eviction.

New PTE methods ([src/mem/pagetable/entry.rs](src/mem/pagetable/entry.rs#L94-L108)):
```rust
pub fn ppn_raw(&self) -> usize { ... }
pub fn set_ppn(&mut self, ppn: usize) { ... }
pub fn clear_flags(&mut self, flags: PTEFlags) { ... }
pub fn set_flags(&mut self, flags: PTEFlags) { ... }
pub fn flags(&self) -> PTEFlags { ... }
```

`ppn_raw`/`set_ppn` read/write the raw PPN field for storing swap indices in invalid PTEs. `clear_flags`/`set_flags` manipulate flag bits atomically. `flags()` exposes the full flag set.

New `PageTable` methods ([src/mem/pagetable.rs](src/mem/pagetable.rs#L91-L106)):
```rust
pub fn get_pte_mut(&mut self, va: usize) -> Option<&mut Entry> { ... }
pub fn unmap(&mut self, va: usize) -> Option<Entry> { ... }
```

`get_pte_mut` provides mutable PTE access for the eviction path. `unmap` clears the V bit and executes `sfence.vma` on the unmapped address.

```rust
pub const USER_STACK_TOP: usize = 0x80500000;
pub const MAX_STACK_SIZE: usize = 8 * 1024 * 1024;
```

Constants defining the user stack region: top at 128 MiB and maximum 8 MiB growth downward.

```rust
pub(super) const USER_POOL_LIMIT: usize = 512;
```

Doubled from 256 to 512 pages (2 MiB) so multiple processes can hold working sets in memory without thrashing.

```rust
pub unsafe fn try_alloc_pages(n: usize) -> Option<*mut u8> { ... }
pub fn available() -> usize { ... }
```

Non-panicking allocation that returns `None` on OOM, and a free-page query. Used by the eviction path to decide when to evict.

#### ALGORITHMS

> C2: In a few paragraphs, describe your code for accessing the data stored in the Supplementary page table about a given page.

The supplementary page table is a `BTreeMap<usize, SuppEntry>` keyed by page-aligned virtual address. All access goes through `SuppPageTable` methods in [src/mem/suppage.rs](src/mem/suppage.rs).

**Lookup**: `get(va)` and `get_mut(va)` align the address down to page boundary (`va & !(PG_SIZE - 1)`) and query the `BTreeMap`. `contains(va)` does the same for existence checks. These are used by the page fault handler to decide how to resolve a fault.

**Insertion**: `insert(va, source)` is called by the ELF loader (for code/data `InFile` entries), by `SYS_MMAP` (for `MmapFile` entries), by `grow_stack` (for `AnonFrame` entries), and by the eviction and swap-in paths (to update entries when pages move between frames and swap).

**Iteration**: `iter()` yields `(&usize, &SuppEntry)` pairs and is used by exit cleanup and overlap detection. For mmap-specific operations, filtering is done by matching on `PageSource` variants — `collect_dirty_writebacks(mapid, pagetable)` finds all dirty mmap pages for a given mapid and copies their data out of physical frames, while `cleanup_mapid(mapid, pagetable, frame_table)` frees frames, unmaps PTEs, and removes supp entries.

**Range queries**: `is_range_free(start, pages)` checks whether `pages` consecutive page slots are all absent from the table, used for overlap detection during mmap.

The table is protected by a per-process `Mutex<SuppPageTable>` (a blocking sleep mutex). The page fault handler acquires this lock briefly to extract a `FaultAction`, then releases it before doing I/O or frame allocation, keeping the critical section short.

> C3: How does your code coordinate accessed and dirty bits between kernel and user virtual addresses that alias a single frame, or alternatively how do you avoid the issue?

I avoid the issue entirely because there is no aliasing. The kernel uses a direct physical-to-virtual identity mapping via `VM_OFFSET` (`0xFFFFFFC000000000`), and user pages are mapped at user virtual addresses through the Sv39 page table. A given physical frame is mapped at exactly one user virtual address (tracked by `FrameEntry::user_va`) and is also accessible to the kernel at `phys + VM_OFFSET`.

When the kernel needs to read or write a page's contents (e.g., during eviction), it accesses the page through the `VM_OFFSET` window. The RISC-V hardware updates the A and D bits in the user PTE — there is no separate kernel PTE for the same frame, so the bits are always read from a single authoritative location. During eviction, `evict_from_current()` reads the D bit from the user PTE via `pagetable.get_pte(va)` before unmapping, then copies the data through the kernel window. Since the frame is unmapped from user space (V cleared) before the frame is freed, no concurrent modification is possible on a single-CPU system.

The `FrameTable::refresh_ad_bits()` method walks all frames belonging to a process, reads the A bit from each frame's corresponding user PTE, and updates `FrameEntry::ref_bit` to feed the clock algorithm with accurate usage information.

#### SYNCHRONIZATION

> C4: When two user processes both need a new frame at the same time, how are races avoided?

Tacos runs on a single CPU with cooperative scheduling — threads only yield the CPU at explicit `schedule()` calls or when blocking on a semaphore. The frame allocator (`UserPool`) is protected by an `Intr` mutex, which disables interrupts during the entire buddy-allocator operation. Since the buddy allocator's critical sections are short (a few linked-list manipulations), the interrupt-disabling lock is appropriate and ensures that a frame allocation is atomic with respect to timer interrupts.

The frame table (`FrameTable`) is also protected by an `Intr` mutex. The eviction path (allocating a frame, choosing a victim, updating supp entries, copying page data, unmapping the PTE, and freeing the old frame) is structured so that I/O (swap reads/writes, file writes) happens outside the `Intr` critical section. The eviction pipeline is:

1. Lock frame table, choose victim, remove from frame table, unlock.
2. Lock pagetable, read D bit, copy page data to static buffer, unlock.
3. Lock supp page table, update supp entry, unlock.
4. Lock pagetable, unmap PTE, unlock.
5. Perform I/O (outside all locks).
6. Free the physical page back to UserPool.

Because only one CPU exists, step 5 (I/O) may block on the virtio semaphore and schedule another thread, but no other thread can interfere with the eviction: the victim frame has already been removed from the frame table, its PTE has been invalidated, and the freeing of the physical page in step 6 happens after I/O completes. If another thread faults on the evicted page before it's freed, it will see the `InSwap`/`MmapFile` supp entry and swap it back in through a new frame.

#### RATIONALE

> C5: Why did you choose the data structure(s) that you did for representing virtual-to-physical mappings?

I chose a two-level design: the hardware Sv39 page table handles active (V=1) mappings, and a per-process `BTreeMap<usize, SuppEntry>` records the backing store for every valid page regardless of whether it currently has a frame.

The hardware page table is dictated by RISC-V — it provides fast hardware walk for active pages. For inactive pages, the PTE's PPN field (when V=0) is repurposed to store a swap index, so the fault handler can recover the swap slot without consulting the supp table. This is an optimization for the cross-process eviction case.

The supplementary page table uses `BTreeMap` rather than a flat array for three reasons. First, virtual address ranges are sparse (code, data, stack, mmap regions are separated by large gaps), so a flat array would waste memory. Second, `BTreeMap` provides ordered iteration, which is useful for `collect_dirty_writebacks` and exit cleanup. Third, `BTreeMap` has O(log n) lookup and insertion, which is fast enough for the page fault path given the small number of entries (at most a few thousand pages per process).

The frame table is a global `BTreeMap<usize, FrameEntry>` keyed by physical address. A global table (rather than per-process) is necessary because physical frames are a global resource. The `keys` vector caches the sorted keys for O(1) indexed access during the clock scan, and `keys_dirty` flags when the cache needs refresh after an insertion or removal.

## Paging To And From Disk

#### DATA STRUCTURES

> D1: Copy here the **declaration** of each new or changed struct, enum type, and global variable. State the purpose of each within 30 words.

```rust
pub struct SwapManager {
    file: Mutex<File>,
    free_slots: Mutex<Vec<usize>>,
}
```

Global swap manager backed by `.glbswap` on disk. `free_slots` is a stack of free slot indices for O(1) allocation and deallocation.

```rust
pub struct Swap;
```

Thin compatibility wrapper around `SwapManager` for test code in `test/`. Delegates `len()`, `page_num()`, and `lock()`.

```rust
struct FrameTable {
    frames: BTreeMap<usize, FrameEntry>,
    clock_hand: usize,
    keys: alloc::vec::Vec<usize>,
    keys_dirty: bool,
}
```

Global frame table (detailed in C1). `clock_hand` indexes into `keys` for the second-chance eviction scan. `keys` caches sorted physical addresses.

```rust
struct FrameEntry {
    pub owner_tid: isize,
    pub user_va: usize,
    pub ref_bit: bool,
    pub pinned: bool,
}
```

Per-frame metadata (detailed in C1). `pinned` prevents a frame from being evicted while the kernel accesses it.

```rust
enum EvictAction {
    WriteSwap { slot: usize },
    WriteFile { file: crate::fs::File, offset: usize },
}
```

Describes the I/O work needed after evicting a page: write to swap slot or write back to a mapped file.

Static eviction buffer ([src/trap/pagefault.rs](src/trap/pagefault.rs#L542)):
```rust
static mut PAGE_BUF: [u8; PG_SIZE] = [0u8; PG_SIZE];
```

A 4 KiB static buffer used during eviction to copy page data before I/O. Placed in `.bss` to avoid consuming kernel stack space.

#### ALGORITHMS

> D2: When a frame is required but none is free, some frame must be evicted. Describe your code for choosing a frame to evict.

When `allocate_or_evict()` ([src/trap/pagefault.rs](src/trap/pagefault.rs#L493-L504)) finds the UserPool exhausted (`try_alloc_pages` returns `None`), it calls `evict_one_frame()`, which delegates to `evict_from_current()`.

`evict_from_current()` calls `FrameTable::evict_one_for(cur_tid)` to select a victim. This method implements the **clock / second-chance algorithm** with a same-process filter:

1. Refresh the cached key vector if dirty.
2. Scan up to `2 * n` entries (two full passes). For each entry:
   - Skip pinned frames (`pinned == true`).
   - Skip frames belonging to other processes (unless `owner_tid == -1` for match-any).
   - If `ref_bit` is 1, clear it to 0 and advance — giving the page a second chance.
   - If `ref_bit` is 0, select this frame as the victim, remove it from the frame table, and return it.
3. If no victim is found after two passes (all frames were referenced or pinned), fall back: scan linearly for any unpinned frame matching the owner filter, ignoring the reference bit.

The `ref_bit` is maintained by two mechanisms: it is set to 1 when a frame is registered (optimistic — assume recent use), and it can be refreshed by `refresh_ad_bits()` which reads the hardware A bit from each frame's PTE and propagates it to `ref_bit`.

Limiting eviction to the current process only (`evict_one_for(cur_tid)`) avoids the cross-process locking complexity that would arise from manipulating another process's page table and supp page table while holding the frame table lock. To ensure each process has enough evictable pages, the parent pre-evicts 16 pages before spawning a child, and the UserPool is 512 pages (2 MiB).

> D3: When a process P obtains a frame that was previously used by a process Q, how do you adjust the page table (and any other data structures) to reflect the frame Q no longer has?

The frame is always evicted by Q itself (same-process eviction), so Q's own eviction path handles all adjustments for Q before the frame is freed:

1. **Copy data out**: Q's `evict_from_current()` copies the page contents into the static `PAGE_BUF` via the kernel's `VM_OFFSET` window.

2. **Update Q's supp entry**: `evict_update_supp()` transitions Q's supp entry based on the page type — `InFrame` becomes `InSwap` (dirty anonymous) or stays `InFile` (clean code), `MmapFile` triggers a file writeback action, etc. The supp entry always reflects where the data went.

3. **Unmap Q's PTE**: `pagetable.unmap(va)` clears the V bit and executes `sfence.vma` on that address, so Q's page table no longer references the physical frame.

4. **I/O**: If the page was dirty, its data is written to swap or to the mapped file. This happens after the PTE is invalidated and the frame table entry is removed.

5. **Free the frame**: `UserPool::dealloc_pages()` returns the physical page to the buddy allocator.

Only after step 5 completes can P (or any process) receive this frame from a subsequent `UserPool::alloc_pages()` call. At that point, the frame has no residual connection to Q — Q's PTE is invalid, Q's supp entry points to the swap slot or file, and the frame table contains no entry for this physical address. P registers the frame afresh in the frame table with its own tid and the new user VA.

The physical page data is safely overwritten when P maps and uses the frame, since the old contents were already preserved to swap/file in step 4.

> D5: Explain the basics of your VM synchronization design. In particular, explain how it prevents deadlock. (Refer to the textbook for an explanation of the necessary conditions for deadlock.)

The VM subsystem uses a two-tier locking strategy: `Intr` mutexes (interrupt-disabling spinlocks) for short-lived global state, and `Mutex` (blocking semaphore-based locks) for per-process data that may be held across I/O.

**Intr-protected resources**: `UserPool` (buddy allocator), `FrameTable`. These locks are never held while doing I/O or while acquiring a `Mutex`. The critical sections are short — a few linked-list operations or a BTreeMap lookup.

**Mutex-protected resources**: Per-process `SuppPageTable` (inside `UserProc::supp_page`), per-process `MmapTable`, pagetable (`cur.pagetable`). These may be held while doing disk I/O (swap reads/writes, file writes during eviction).

Deadlock is prevented by two design rules:

1. **No circular waiting**: `Intr` locks are never acquired while holding a `Mutex`. The locking order is always: acquire `Mutex` (supp_page, pagetable) → release `Mutex` → optionally acquire `Intr` (FrameTable, UserPool) → release `Intr`. I/O happens with no locks held.

2. **Hold-and-wait is broken**: Long operations (eviction, mmap writeback, exit cleanup) are structured as a pipeline. The code locks, extracts needed information, unlocks, then acts on it. For example, the exit path collects writeback data with the pagetable locked, releases the lock, performs file I/O (which may block on the virtio semaphore), then re-acquires the lock for cleanup. The eviction path locks the frame table only long enough to remove the victim entry, then releases it before doing swap I/O.

The textbook's four necessary conditions for deadlock are: mutual exclusion, hold-and-wait, no preemption, and circular wait. By ensuring that locks are never nested across the `Intr`/`Mutex` boundary and by always releasing locks before blocking on I/O, both hold-and-wait and circular wait are avoided.

> D6: A page fault in process P can cause another process Q's frame to be evicted. How do you ensure that Q cannot access or modify the page during the eviction process? How do you avoid a race between P evicting Q's frame and Q faulting the page back in?

In the current implementation, eviction is **same-process only**: `evict_from_current()` calls `FrameTable::evict_one_for(cur_tid)`, which only considers frames whose `owner_tid` matches the current thread. So P never evicts Q's frames directly.

However, the design handles the theoretical cross-process case as follows. The eviction pipeline unmaps the PTE (clears V, executes `sfence.vma`) before the physical frame is freed. On a single-CPU system, Q cannot access the page between the unmap and the free because Q is not running — only P is. The `sfence.vma` ensures that any stale TLB entries are flushed.

If Q later faults on the evicted page, it will consult its supplementary page table, find the page is now in swap (or still file-backed), and swap it back in through a different physical frame. The original frame is long gone. The race between "P begins evicting Q's page" and "Q faults the same page back in" is avoided by the Intr mutex on the frame table: the victim selection and removal is atomic, and once removed, the frame cannot be selected again.

> D7: Suppose a page fault in process P causes a page to be read from the file system or swap. How do you ensure that a second process Q cannot interfere by e.g. attempting to evict the frame while it is still being read in?

First, as noted in D6, Q can only evict its own frames due to same-process eviction. Q cannot evict P's newly allocated frame.

Second, even within P, the frame is registered in the frame table immediately after `load_from_file()` or `swap_in()` maps the PTE, and before I/O is complete — but the I/O is already done by that point. The allocation, I/O, mapping, and registration happen sequentially in `load_from_file()`:

1. `allocate_or_evict()` returns a page.
2. File data is read into the page.
3. `pagetable.map()` sets up the PTE with V=1.
4. `FrameTable::register()` inserts the frame entry.

If P takes another page fault concurrently (which can't happen on a single CPU), or if P's thread is preempted between steps 1 and 4, no other thread can steal or evict this frame because it hasn't been registered in the frame table yet — the clock algorithm only scans registered frames. The physical page belongs to the buddy allocator's "allocated" set, not the free set, so it won't be handed out again.

> D8: Explain how you handle access to paged-out pages that occur during system calls. Do you use page faults to bring in pages (as in user programs), or do you have a mechanism for "locking" frames into physical memory, or do you use some other design? How do you gracefully handle attempted accesses to invalid virtual addresses?

I use page faults to bring in pages during system calls. The mechanism leverages the existing `__knrl_read_usr_byte` / `__knrl_write_usr_byte` assembly stubs from Lab 2.

When a syscall (e.g., `read`) needs to write into a user buffer whose page has been swapped out, the `write_user_buf` helper calls `__knrl_write_usr_byte`, which executes an `sb` instruction. If the target page is not present (V=0), a StorePageFault occurs in supervisor mode. The page fault handler recognizes the faulting PC (`frame.sepc == __knrl_write_usr_byte_pc`), and `handle_kernel_user_fault()` attempts to resolve it:

1. Check the supplementary page table for the faulting address.
2. If `InFile` or `MmapFile`: call `load_from_file()`.
3. If `InSwap`, `MmapSwap`, or `AnonSwap`: call `swap_in()`.
4. If a stale PTE stores a swap index: recover and swap in.
5. If the address is in the stack region and unmapped: call `grow_stack()`.
6. If none of the above: return `false`, the handler signals an error, and the syscall returns -1.

This design reuses the same fault resolution logic for both user-mode and kernel-mode faults, keeping the codebase compact. The difference is only in error handling: a user-mode fault that can't be resolved kills the process, while a kernel-mode fault returns an error to the syscall.

For **invalid virtual addresses**, the pre-validation in `is_valid_area()` checks that each page in `[start, end)` either has a valid PTE (V=1) or exists in the supplementary page table. Addresses that fail both checks cause the syscall to return -1 immediately. If the address passes the coarse check but still faults (e.g., the page was evicted between the check and the access), the bytewise helper catches the fault and returns an error, which propagates to -1.

The `FrameEntry::pinned` field provides a mechanism to lock frames into memory, though the current implementation relies on fault-and-retry rather than pre-pinning.

> D9: A single lock for the whole VM system would make synchronization easy, but limit parallelism. On the other hand, using many locks complicates synchronization and raises the possibility for deadlock but allows for high parallelism. Explain where your design falls along this continuum and why you chose to design it this way.

My design falls closer to the "few coarse locks" end of the spectrum, which is appropriate for a single-CPU kernel.

The global state (`FrameTable` and `UserPool`) uses per-data-structure `Intr` mutexes rather than a single VM lock. This is still coarse — every frame allocation and eviction contends on the same frame table lock — but it separates concerns: the buddy allocator lock and the frame table lock are distinct, so a thread freeing a page doesn't block another thread from looking up a frame.

The per-process state (`SuppPageTable`, `MmapTable`, page table) uses separate `Mutex` instances per process. This allows one process to handle a page fault (holding its own supp_page lock) while another process is blocked on I/O. However, the single-CPU nature means true parallelism doesn't exist — the benefit is liveness, not throughput.

I chose this design because:

1. **Simplicity**: The locking rules are easy to verify: Intr locks are never nested, Mutex locks are held only briefly, and no lock is held across I/O. This makes deadlock reasoning straightforward.

2. **Single-CPU reality**: Fine-grained locking with multiple locks per page or per frame would add complexity (lock ordering, deadlock avoidance) with no throughput benefit on a uniprocessor.

3. **Teaching context**: The Pintos/Tacos project values clarity and correctness over performance. A simpler locking scheme is easier for TAs to review and for students to understand.

The main concession to liveness is the pipeline structure: collect-then-act patterns keep critical sections short, and I/O always happens outside locks. If Tacos were extended to multi-core, the frame table and UserPool locks would become contention points, and a per-core frame pool or lock-free allocator would be warranted.

## Rationale

> E1: Have you considered other design possibilities? You can talk about anything in your solution that you once thought about doing them another way. And for what reasons that you made your choice?

**Cross-process eviction vs. same-process eviction**: I initially implemented full cross-process eviction — the clock algorithm scanned all frames regardless of owner, and evicting a frame required locking the owning process's page table and supp page table. This caused `Intr` double-lock panics because the sleep mutex's release path calls `cur.donors.lock()`, which is also an `Intr` mutex. Debugging this was time-consuming and the fix required restructuring the entire locking hierarchy. I switched to same-process-only eviction, which avoids the problem entirely: a process only manipulates its own page table and supp page table during eviction. To prevent unfairness, the UserPool was doubled to 512 pages and the parent pre-evicts before spawning children.

**Bitmap vs. free stack for swap slots**: The initial design used a bitmap with linear scan for swap slot allocation. I switched to a `Vec<usize>` free stack, where `alloc_slot()` pops from the end and `free_slot()` pushes back — both O(1). This is simpler and faster than a bitmap scan, at the cost of a few hundred bytes of memory for the stack.

**Double buffering in swap_in**: The first version of `swap_in()` allocated a temporary kernel buffer, called `SwapManager::read_slot()` into that buffer, then copied the buffer into the newly allocated physical page. I eliminated the intermediate buffer by reading directly into the allocated page (which is accessible via `VM_OFFSET`), cutting the copy work in half.

**Eager zero-fill in load_from_file**: The initial `load_from_file()` zero-filled the entire page before reading file data into it. Since the read covers the first `filesz` bytes and only the tail needs zeroing, I changed it to zero-fill only `page[readsz..]`, avoiding redundant work.

**Static buffer for eviction**: The eviction path originally used a `[u8; PG_SIZE]` local variable for the page copy buffer, which, combined with the deep call chain from page fault handling, overflowed the 16 KiB kernel stack. Moving the buffer to a `static mut` in `.bss` and doubling `STACK_SIZE` to 32 KiB resolved the stack overflows. The unsafe static is acceptable because the single-CPU design ensures no concurrent access.
