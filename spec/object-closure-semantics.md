# Structs, Methods, Functions, and Closures

Status: draft

This document defines the runtime behavior for the object and callable instructions in `spec/instruction-semantics.md`. It builds on the function-frame and closure ABI in `spec/function-abi.md`.

## Structs and instances

`STRUCT_DEF` registers one struct ID and its ordered field IDs in the current module. Struct IDs and field IDs are nonnegative `Int` values. A struct ID and each field ID must be unique within their respective definitions. Struct and interface IDs share one type-ID namespace and cannot collide. All declared instance fields are mutable and begin with `Null`.

`METHOD_DEF` attaches a function-table function to a struct under a method ID. Method IDs are unique within a struct. Method ID `0` is reserved for the constructor. A constructor is an ordinary synchronous function whose first declared parameter receives the new instance; its remaining parameters receive the arguments passed to `OBJECT_NEW`. The constructor's return value is discarded. `OBJECT_NEW` pushes the created instance after the constructor returns. If no constructor is declared, the argument count must be zero.

`FIELD_GET` and `FIELD_SET` access only declared instance fields. `METHOD_CALL` prepends the receiver to the ordered explicit arguments before binding the callee's parameters. Calling an absent field or method raises `ReferenceError`; using a non-instance receiver raises `TypeError`.

`INTERFACE_DEF` registers a set of required method IDs. `IMPLEMENT` succeeds only when the struct has every listed method. `OBJECT_IS` returns true for the object's struct ID or an interface explicitly recorded by `IMPLEMENT`; it does not infer conformance from method shape. `OBJECT_CLONE` creates a distinct instance with the same struct ID and a shallow copy of its field values. Object equality compares instance identity.

Static fields and static methods are not implemented by this runtime slice. They require separate storage and call semantics and are deferred.

## Function values and closures

A `Function` value refers to a function-table entry by module index and function ID. A `Closure` pairs that reference with an ordered vector of captured `(variable ID, shared binding cell)` pairs. Function values compare by module index and function ID; closures compare by identity.

Each execution frame owns a pending capture vector. `CAPTURE` resolves the nearest in-scope binding (falling back to a module global), rejects duplicate IDs in that vector, and appends the binding's shared cell. `LAMBDA` validates its body function and parameter count, consumes the vector, and creates a closure. Calls to closures install the captured cells in a lexical scope before the parameter scope. Nested and recursive frames receive independent pending vectors; assignments to a captured mutable binding remain visible through every closure that shares its cell.

`CALL_VALUE` invokes a `Function` or `Closure` using the function ABI's argument order and arity rules. The current source interpreter and bytecode VM do not provide native-function values or asynchronous task scheduling; async calls and native callables fail explicitly until their respective runtime features are implemented. The bytecode VM currently accepts one module per image.

## Runtime display

Objects use the `Struct#ID` display form, functions use `Function#ID`, and closures use `Closure#BODY_ID`, as defined by `spec/type-semantics.md`. Object and closure displays do not recursively print fields or captured values.
