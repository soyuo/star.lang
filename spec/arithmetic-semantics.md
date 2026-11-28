# Arithmetic and Numeric Semantics

Status: stable

This document defines arithmetic, numeric comparison, integer division and remainder, mixed `Int`/`Float` operations, and bitwise behavior. The runtime types and conversions used here are defined in `spec/type-semantics.md`; instruction stack effects are in `spec/instruction-semantics.md`; source integer and byte encodings are in `spec/integer-encoding.md`. This is source-language behavior and does not define `.starc` opcodes. Examples use infix mathematical notation only; Star executes postfix instructions.

## Numeric operands and results

Arithmetic and ordered numeric comparisons accept `Int` and `Float`; `MOD` is the exception and accepts only two `Int` operands. `Byte` is a distinct type and is not an arithmetic operand; convert it explicitly with `TO_INT` or `TO_FLOAT`. `Bool` and numeric-looking `String` values are not arithmetic operands. A statically provable operand-type violation is a `CompileError`; a mismatch discovered at runtime is a `TypeError`. Value-domain failures such as division by zero and a negative shift count use their stated runtime error.

| Operand types | Operation/result type |
|---|---|
| `Int`, `Int` | Integer operations return arbitrary-precision `Int`, except negative-exponent `POW` as specified below. |
| `Float`, `Float` | Floating operations return `Float`. |
| `Int`, `Float` or `Float`, `Int` | For operators that accept mixed operands, convert the `Int` using `TO_FLOAT`, then perform the operation as `Float`; `MOD` rejects mixed operands. |

Mixed operations therefore use the binary64 rounding and overflow rules of `TO_FLOAT`; a large `Int` may round or convert to infinity before the operation. No operator silently converts `Bool`, `Byte`, `Char`, or `String` to a number.

## Integer arithmetic and overflow

`Int` is signed and arbitrary precision. `ADD`, `SUB`, `MUL`, `NEG`, `ABS`, and nonnegative integer `POW` produce the exact mathematical result as `Int`; they never wrap, clamp, or promote to `Float` because an integer result is large. The implementation may use a small-integer representation internally, but that representation must promote without changing observable results.

An operation may fail with `OutOfMemoryError` or the configured execution/resource limit if the result or computation exceeds available resources. Such limits do not authorize returning a wrapped, clamped, or approximate `Int`.

## Division and remainder

For `DIV` with two `Int` operands, the quotient is truncated toward zero and the result is `Int`. For `DIV` with at least one `Float`, both operands are treated as `Float` and IEEE 754 binary64 division is used. A divisor equal to integer zero or either floating zero (`+0.0` or `-0.0`) raises `DivisionByZero`; this rule also applies when the numerator is NaN or infinity.

`MOD` accepts only two `Int` operands. Let `q` be the `Int` quotient of `a / b` truncated toward zero; the result is `r = a - q*b`. Thus `|r| < |b|`, and a nonzero remainder has the sign of the dividend. Examples: `-7 MOD 3 = -1`, `7 MOD -3 = 1`, and `-7 MOD -3 = -1`. A zero divisor raises `DivisionByZero` at runtime; a `Float` operand raises `TypeError` at runtime or `CompileError` when its type is statically known.

## Floating-point arithmetic

`ADD`, `SUB`, `MUL`, and floating `DIV` operate on IEEE 754 binary64 values and round each result to nearest, ties to even. Finite overflow yields the corresponding infinity; underflow yields the correctly signed subnormal or zero. NaN and infinity behavior follows binary64 arithmetic, except for the explicit `DivisionByZero` rule above. Negative zero is preserved where required by binary64 operations and remains observable through `TO_STRING`.

## Exponentiation, negation, and absolute value

`POW` accepts `Int` and `Float` operands. With two `Int` operands and a nonnegative exponent it returns the exact integer power; `0 POW 0` is `1`. With two `Int` operands and a negative exponent, zero as the base raises `DivisionByZero`; bases `1` and `-1` return their exact `Int` result, and every other base returns a `Float` formed by rounding the exact rational power to binary64 using round-to-nearest, ties-to-even. Underflow preserves the sign of that exact result. If either operand is `Float`, both operands are treated as `Float` and the result is `Float`; `0.0 POW 0.0` is `1.0`, while zero to a negative exponent raises `DivisionByZero`. A negative finite base raised to a finite, non-integral floating exponent produces NaN. Other floating results follow IEEE 754 binary64 power special cases; real-valued results round to nearest, ties to even, with overflow or underflow producing the corresponding infinity, subnormal, or signed zero.

`NEG` and `ABS` accept `Int` or `Float`. Integer negation and absolute value are exact and cannot overflow. Floating `NEG` changes the sign, including the sign of zero and NaN; floating `ABS` clears the sign, so `ABS(-0.0)` is `+0.0` and `ABS(NaN)` is NaN.

## Ordered numeric comparison

`GT` and `LT` accept `Int` and `Float`. Same-type integers compare exactly; same-type floats use binary64 ordered comparison. Mixed operands compare after converting the `Int` with `TO_FLOAT`, so a large integer can lose precision in a mixed comparison. A comparison involving NaN with `GT` or `LT` returns `false`. Other types raise `TypeError`.

## Bitwise operations and shifts

`BIT_AND`, `BIT_OR`, `BIT_XOR`, `BIT_NOT`, `SHL`, and `SHR` accept only `Int`. Bitwise operators use infinite-width two's-complement semantics with sign extension: `BIT_NOT(x) = -x - 1`. `SHL(x, n)` for nonnegative `n` is the exact integer `x * 2^n`; it does not wrap. `SHR(x, n)` is arithmetic right shift, equivalent to floor division by `2^n` (so `-7 SHR 1 = -4`). A negative shift count raises `TypeError` at runtime because it violates the value domain; a non-`Int` count is a `CompileError` when statically known, otherwise `TypeError` at runtime. Large left shifts may fail at configured resource limits, but may not return a wrapped result.
