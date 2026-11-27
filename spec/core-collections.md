# Core Collections

Status: draft

Star's guest-visible dynamic arrays, hash maps, and hash sets are the `Array`, `Map`, and `Set` values defined in [collection semantics](collection-semantics.md). They provide mutable zero-based arrays, value-keyed hash lookup, deterministic insertion-order iteration, stable replacement positions, and the documented empty/error behavior. Their map and set implementation uses hashing for membership and a separate order sequence for deterministic traversal.

The interpreter also exposes `star_interpreter::runtime::collections` primitives for the additional standard collection types:

| Type | Order and operations | Empty behavior | Expected operation cost |
|---|---|---|---|
| `Stack<T>` | LIFO; `try_push`, `pop`, `peek`; iteration is bottom to top | `pop` and `peek` return `None` | push amortized `O(1)`; pop/peek `O(1)` |
| `Queue<T>` | FIFO; `try_enqueue`, `dequeue`, `front`; iteration is oldest to newest | `dequeue` and `front` return `None` | enqueue/dequeue/front `O(1)` amortized or better |
| `Deque<T>` | Double-ended; `try_push_front`, `try_push_back`, `pop_front`, `pop_back`, `front`, `back`; iteration is front to back | pops and peeks return `None` | end operations `O(1)` amortized or better |

Push/enqueue operations reserve before mutation and return `TryReserveError` if allocation cannot be reserved. Removal and peek on an empty structure are non-errors. Iteration preserves the order stated above; values are never sorted implicitly.

## Star standard-library boundary

The Rust collection primitives are implemented now, while guest module loading and native module registration are a later roadmap item. When exposed through `std.collections`, empty `pop`/`dequeue`/`peek` results map to Star `Null`; allocation and configured-memory-limit failures map to the runtime's resource-limit error. Guest bindings must account for backing storage under `max_memory_bytes`. This boundary keeps the host implementations available without claiming that `import std.collections` already works.
