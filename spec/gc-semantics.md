# Tracing Garbage Collection

Status: stable

The reference interpreter uses a non-moving tracing collector for managed runtime values. Collection preserves object identity and does not relocate values.

## Managed values and edges

Arrays, tuples, maps, sets, iterators, closures, and objects are managed nodes. Their outgoing edges are their contained values and, for closures, captured binding cells. Map and set bookkeeping keys are not additional runtime-value edges; duplicated values held in both a lookup index and iteration order are counted as separate handles.

All interpreter-created nodes are registered through the interpreter heap. The registry stores weak handles, so it does not keep an otherwise unreachable node alive. Before sweeping an unreachable node, the collector clears its outgoing edges. This breaks reference-counted cycles while preserving stable `Rc` handles for every live value.

## Roots and collection

The root set includes the operand stack, all active lexical and module bindings, pending closure captures, and active frame state. Host-owned strong handles that remain alive while the interpreter heap exists are treated as roots as well. Tracing follows values in arrays, tuples, maps, sets, iterators, objects, and captured binding cells, with visited-node and visited-cell sets to handle cycles.

Collection runs after successful source or bytecode instructions when the allocation threshold is reached. The default threshold is 64 newly allocated managed nodes. After collection, the next threshold is twice the live-node count, with a minimum of 64; hosts may choose a different initial threshold. Hosts can request a collection explicitly through `Interpreter::collect_garbage` or `BytecodeVm::collect_garbage`. The returned `GcStats` reports node counts, reclaimed nodes, total allocations, completed collections, and the next threshold. If a host currently holds a `RefCell` borrow of a reachable runtime value, explicit collection returns `GcError::BorrowConflict` without changing the heap. Automatic collection defers and retries after a later instruction.

An unreachable node is reclaimed only after its registered edges are cleared and all temporary collector handles are dropped. Live values retain identity; tuples remain immutable to Star programs even though the collector may clear the storage of an unreachable tuple to break a cycle.

## Limits and host resources

Collection does not replace bytecode verification limits, execution-step limits, call-depth limits, or host operating-system isolation. External resources such as files, sockets, and native handles require explicit cleanup and are not reclaimed by this collector.
