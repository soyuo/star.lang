# Concurrency Runtime

Status: stable

## Cooperative tasks

The reference source interpreter and bytecode VM execute guest code on one
thread. `ASYNC_FUNC` creates a task; `SPAWN` schedules a callable; and `AWAIT`,
`JOIN`, `YIELD`, `SLEEP`, channel operations, and contended mutex locks run
ready tasks cooperatively. Ordinary guest instructions do not preempt a running
task. The scheduler selects ready tasks in FIFO order. If a blocking operation
has no ready task that can make progress, it raises `Deadlock`.

Task failures are retained by the task and are raised when the task is awaited
or joined. A task releases mutexes it still owns on normal return or failure.
Cancelling a pending task prevents its body from running; a task already
running cannot be cancelled.

## Channels

Channels preserve FIFO message order. A positive capacity is the number of
values that may wait in the channel. Sending to a full channel and receiving
from an empty channel run other ready tasks until the operation can proceed or
the scheduler reports `Deadlock`.

A zero-capacity channel has no buffered queue. A send can complete only after a
receiver has registered as waiting. At that handoff point, the sender and the
matched receive are logically complete; the value is stored in a transfer slot
owned by that receiver until its suspended call resumes. Waiters are paired in
FIFO order. The sender does not add the value to a reusable channel buffer.

Channels have no close operation in this instruction set. A channel handle
must refer to a channel created by the same runtime instance.

## Mutexes

Mutexes are non-reentrant and owned by the current task. A contended lock runs
other ready tasks until the mutex becomes available or `Deadlock` is raised.
Only the owning task can unlock a mutex; a wrong-owner unlock raises
`PermissionError`. Any mutexes still owned are released when a task exits.

## Workers and native interfaces

Worker execution uses an isolated VM and transfers only supported serialized
messages; worker tasks do not share guest heap objects. Native FFI operations
and callbacks require the `ffi` capability. FFI callbacks are synchronous,
scalar-only trampolines and can invoke only a synchronous `Function` or
`Closure`. They must run on the thread currently executing the native call and
only before that call returns. A wrong-thread invocation is rejected and
causes the native call to fail with `TypeError`; callback runtime errors are
propagated after native code returns and never unwind through C. Native code
must not retain a callback pointer after the call or invoke it concurrently.
Async guest functions cannot serve as synchronous native callbacks.
