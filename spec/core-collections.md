# Core Collections

Status: stable

Star's guest-visible dynamic arrays, hash maps, and hash sets are the `Array`, `Map`, and `Set` values defined in [collection semantics](collection-semantics.md). They provide mutable zero-based arrays, value-keyed hash lookup, deterministic insertion-order iteration, stable replacement positions, and the documented empty/error behavior. Their map and set implementation uses hashing for membership and a separate order sequence for deterministic traversal.

The interpreter also exposes `star_interpreter::runtime::collections` primitives for the additional standard collection types:

| Type | Order and operations | Empty behavior | Expected operation cost |
|---|---|---|---|
| `Stack<T>` | LIFO; `try_push`, `pop`, `peek`; iteration is bottom to top | `pop` and `peek` return `None` | push amortized `O(1)`; pop/peek `O(1)` |
| `Queue<T>` | FIFO; `try_enqueue`, `dequeue`, `front`; iteration is oldest to newest | `dequeue` and `front` return `None` | enqueue/dequeue/front `O(1)` amortized or better |
| `Deque<T>` | Double-ended; `try_push_front`, `try_push_back`, `pop_front`, `pop_back`, `front`, `back`; iteration is front to back | pops and peeks return `None` | end operations `O(1)` amortized or better |
| `MinPriorityQueue<T>` | Smallest value first; equal values are removed in insertion order | `pop` and `peek` return `None` | push/pop `O(log n)`; peek `O(1)` |
| `MaxPriorityQueue<T>` | Largest value first; equal values are removed in insertion order | `pop` and `peek` return `None` | push/pop `O(log n)`; peek `O(1)` |

Push/enqueue operations reserve before mutation and return `TryReserveError` if allocation cannot be reserved. Removal and peek on an empty structure are non-errors. Iteration preserves the order stated above; values are never sorted implicitly.

The module also provides `sort` (ascending, unstable), `stable_sort` (ascending, preserving the relative order of equal values), `binary_search`, `lower_bound`, and `upper_bound` over sorted slices. They use the element type's total `Ord` order. `binary_search` returns the first equal index or the lower-bound insertion point; `lower_bound` finds the first element not less than the target; `upper_bound` finds the first element greater than the target. Sorting takes `O(n log n)` time; unstable sort uses `O(log n)` stack space while stable sort may use `O(n)` auxiliary space. Searches take `O(log n)` time and `O(1)` extra space.

## Star standard-library boundary

The Rust collection primitives are implemented. Star 1.0 does not register them as a guest `std.collections` module, so `import std.collections` is not part of the 1.0 surface. If a host exposes these APIs to guest code, empty `pop`/`dequeue`/`peek` results map to Star `Null`; allocation and configured-memory-limit failures map to the runtime's resource-limit error. Guest bindings must account for backing storage under `max_memory_bytes`.
