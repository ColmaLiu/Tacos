# Lab 1: Scheduling

---

## Information

Name: Yunuo Liu

Email: liuyunuo@stu.pku.edu.cn

> Please cite any forms of information source that you have consulted during finishing your assignment, except the TacOS documentation, course slides, and course staff.

> With any comments that may help TAs to evaluate your work better, please leave them here

## Alarm Clock

### Data Structures

> A1: Copy here the **declaration** of every new or modified struct, enum type, and global variable. State the purpose of each within 30 words.

```rust
pub struct Manager {
    /// ...
    /// Sleeping threads sorted by wakeup tick
    pub sleep_queue: Mutex<SleepQueue>,
}
```

A global sleep queue in `Manager`, protected by a mutex. It stores blocked threads ordered by wake-up tick for efficient timer-based wake-up.

```rust
pub struct SleepQueue(BTreeMap<i64, Vec<Arc<Thread>>>);
```

A sorted map from wake-up tick to all threads that should be awakened at that tick.

### Algorithms

> A2: Briefly describe what happens in `sleep()` and the timer interrupt handler.

In `sleep(ticks)`, interrupts are first disabled so the sleep operation becomes atomic. The current timer tick is read and a `wakeup_tick` is computed. If `ticks > 0`, the current thread is inserted into `Manager::sleep_queue` by calling `sleep_until(current.clone(), wakeup_tick)`, and its status is changed to `Blocked`. Then the previous interrupt state is restored and `schedule()` is called so another runnable thread can run. If `ticks <= 0`, the thread is not inserted into the sleep queue and simply yields through `schedule()`.

In the timer interrupt handler, the kernel first updates the timer state by calling `sbi::timer::tick()`, then reads the current tick with `sbi::timer::timer_ticks()`. It removes all expired sleeping threads from the sleep queue by calling `wake_expired(now)`. For every returned thread, it calls `thread::wake_up(thread)` to make the thread runnable again. After that, interrupts are enabled and `thread::schedule()` is invoked so the scheduler may run a newly awakened thread.

> A3: What are your efforts to minimize the amount of time spent in the timer interrupt handler?

The main optimization is the sleep queue data structure. `SleepQueue` is implemented as a `BTreeMap<i64, Vec<Arc<Thread>>>`, so sleeping threads are kept sorted by wake-up tick. Therefore, in the timer interrupt handler, `wake_expired(now)` only examines the earliest wake-up entries and stops as soon as it reaches the first tick that is greater than the current time. This avoids scanning all blocked threads on every timer interrupt.

In addition, threads with the same wake-up tick are grouped together in a `Vec`, so they can be removed together with one map entry. The handler only performs three small tasks: advance the timer, collect expired sleepers, and wake them up. This keeps interrupt-side work short and predictable.

### Synchronization

> A4: How are race conditions avoided when `sleep()` is being called concurrently?

Race conditions are avoided by combining interrupt disabling and mutual exclusion.

First, `sleep()` disables interrupts before reading the current tick, inserting the thread into the sleep queue, and changing the thread status to `Blocked`. This ensures that the current execution cannot be interrupted in the middle of the sleep procedure.

Second, the shared sleep queue is protected by `Mutex<SleepQueue>`, so concurrent calls to `sleep()` cannot modify the queue simultaneously. Each sleeping thread is inserted into the queue in a serialized way, which prevents corruption of the queue state.

> A5: How are race conditions avoided when a timer interrupt occurs during a call to `sleep()`?

`sleep()` disables interrupts at the very beginning by calling `sbi::interrupt::set(false)`. Because of that, a timer interrupt cannot occur while the thread is in the critical section where it computes `wakeup_tick`, inserts itself into the sleep queue, and changes its status to `Blocked`.

This guarantees that the timer interrupt handler never observes an inconsistent intermediate state. The thread is either not yet sleeping at all, or already fully inserted into the sleep queue and marked `Blocked`. Thus, the handler cannot miss a sleeping thread, and it also cannot wake a thread that has not finished going to sleep.

## Priority Scheduling

### Data Structures

> B1: Copy here the **declaration** of every new or modified struct, enum type, and global variable. State the purpose of each within 30 words.

```rust
pub struct Thread {
    /// ...
    pub priority: AtomicU32,
    pub effective_priority: AtomicU32,
    pub waiting_thread: Mutex<Option<Arc<Thread>>>,
    pub donors: Mutex<Vec<Arc<Thread>>>,
}
```

- `priority`: The thread’s base priority set by the user.
- `effective_priority`: The thread’s current priority after considering priority donation.
- `waiting_thread`: The thread currently holding the lock this thread is waiting for.
- `donors`: Threads that are currently donating priority to this thread.

```rust
pub struct Priority(BTreeMap<u32, VecDeque<Arc<Thread>>>);
```

`Priority` scheduler stores ready threads grouped by effective priority. The scheduler always chooses from the highest-priority nonempty queue.

```rust
pub struct Semaphore {
    value: Cell<usize>,
    waiters: RefCell<BTreeMap<u32, VecDeque<Arc<Thread>>>>,
}
```

Semaphore waiter queue stores blocked semaphore waiters grouped by effective priority so the highest-priority waiter is awakened first.

```rust
pub struct Condvar(RefCell<BTreeMap<u32, VecDeque<Arc<Semaphore>>>>);
```

Condition variable waiter queue stores condition-variable waiters grouped by priority. Each waiting thread is represented by a private semaphore.

> B2: Explain the data structure that tracks priority donation. Clarify your answer with any forms of diagram (e.g., the ASCII art).

Priority donation is tracked mainly by two thread fields:

- `waiting_thread`: points to the thread that owns the lock this thread is currently waiting for.
- `donors`: stores threads that are donating priority to this thread.

This forms a donation chain. If a high-priority thread waits on a lock held by a lower-priority thread, the low-priority holder inherits the higher effective priority. If that holder is itself waiting on another lock, the donation continues recursively through `waiting_thread`.

For example:

```text
T1 (priority 50) waits for lock held by T2
T2 (priority 20) waits for lock held by T3
T3 (priority 10) is running/ready

Donation chain:

T1 --waiting_thread--> T2 --waiting_thread--> T3

donors lists:
T2.donors = [T1]
T3.donors = [T2]

effective priorities:
T1 = 50
T2 = 50
T3 = 50
```

So the data structure is not a separate graph object. Instead, it is represented implicitly by per-thread `waiting_thread` pointers and `donors` lists, which together encode nested donation relationships.

### Algorithms

> B3: How do you ensure that the highest priority thread waiting for a lock, semaphore, or condition variable wakes up first?

For semaphores, waiters are stored in a `BTreeMap<u32, VecDeque<Arc<Thread>>>`, keyed by effective priority. In `Semaphore::up()`, the implementation first calls `rearrange()` to move any thread whose effective priority changed into the correct priority bucket. Then it selects `last_key_value()`, which is the highest priority key in the BTreeMap, and wakes one thread from that queue. FIFO order is preserved among threads with the same priority by using `push_front()` when blocking and `pop_back()` when waking.

For locks, the lock is implemented on top of a semaphore, so lock waiters are awakened according to the semaphore’s highest-priority-first rule.

For condition variables, each waiting thread is represented by a private semaphore stored in a `BTreeMap<u32, VecDeque<Arc<Semaphore>>>`. `notify_one()` selects the highest priority key using `last_key_value()` and signals one semaphore from that queue, so the highest-priority waiter is awakened first.

> B4: Describe the sequence of events when a thread tries to acquire a lock. How is nested donation handled?

When a thread calls `Sleep::acquire()`:

1. Interrupts are disabled.
2. The current thread is obtained.
3. If the lock already has a holder, the current thread sets its `waiting_thread` to that holder.
4. Then the code walks along the `waiting_thread` chain:
    - let `holder` be the thread currently being waited on,
    - compare the current donor thread’s `effective_priority` with `holder.effective_priority`,
    - if the donor is higher, raise the holder’s `effective_priority`,
    - append the donor thread to `holder.donors`,
    - continue if that holder is itself waiting on another thread.
5. After donation propagation, the ready queue is rearranged so scheduler order reflects any new effective priorities.
6. The thread performs `self.inner.down()` to actually wait for the lock if necessary.
7. Once awakened and the semaphore is acquired, it becomes the lock holder.

Nested donation is handled by the `while t.waiting_thread.lock().is_some()` loop. This recursively propagates donation through multiple lock dependencies until the chain ends.

> B5: Describe the sequence of events when a lock, which a higher-priority thread is waiting for, is released.

When `Sleep::release()` is called:

1. The implementation first checks that the current thread really holds the lock.
2. Interrupts are disabled.
3. It collects the current semaphore waiters by calling `self.inner.waiters()`.
4. From the current thread’s `donors` list, it removes all donor threads that are waiting for this lock. For each removed donor, its `waiting_thread` is cleared.
5. The current thread recomputes its own `effective_priority` as the maximum of:
    - its original `priority`, and
    - the effective priorities of any remaining donors.
6. Then this recomputation is propagated upward through the `waiting_thread` chain, so if the current thread had donated to another holder, that holder’s effective priority is also updated.
7. The scheduler is rearranged to reflect the updated priorities.
8. The lock holder field is cleared.
9. `self.inner.up()` is called, which wakes the highest-priority waiter on the underlying semaphore.
10. After waking a waiter, scheduling may occur, allowing the newly unblocked high-priority thread to run.

Thus, releasing a lock removes only the donations associated with that lock, recomputes priorities, and wakes the highest-priority waiting thread.

### Synchronization

> B6: Describe a potential race in `thread::set_priority()` and explain how your implementation avoids it. Can you use a lock to avoid this race?

A potential race occurs if `set_priority()` changes a thread’s base priority while donation-related state is being changed at the same time, for example during `acquire()` or `release()`. Without protection, the thread’s `priority`, `effective_priority`, donor list, and scheduler position could become inconsistent. For instance, `effective_priority` might be recomputed using an outdated donor set, or the scheduler might still keep the thread in the wrong priority queue.

My implementation avoids this race by disabling interrupts during the entire `set_priority()` operation. Inside the critical section, it:

1. updates the base `priority`,
2. recomputes the current thread’s `effective_priority` from its donors,
3. propagates any necessary priority changes along the `waiting_thread` chain,
4. rearranges the scheduler queues if priority scheduling is enabled.

Because interrupts are disabled, this sequence is atomic with respect to thread blocking, waking, donation propagation, and scheduling on the current CPU.

A lock should not be used here to avoid the race. The priority-setting code may run in contexts where sleeping is not allowed, and using a lock could itself require blocking or create new priority inversion problems. Also, the race involves scheduling and interrupt-time interactions, so interrupt disabling is the correct mechanism.

## Rationale

> C1: Have you considered other design possibilities? You can talk about anything in your solution that you once thought about doing them another way. And for what reasons that you made your choice?

For priority donation, I considered recomputing donation relationships globally whenever a lock is acquired or released. That approach might be conceptually cleaner, but it would require scanning many threads and locks, which is more complicated and less efficient. Instead, I used per-thread fields `waiting_thread` and `donors` to represent the donation chain incrementally. This makes nested donation natural: when a thread waits for a lock, the code can simply walk through the `waiting_thread` chain and propagate the donated priority upward.
