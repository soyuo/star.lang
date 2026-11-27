# Integer Math

Status: draft

`star_interpreter::runtime::math` provides exact integer helpers for the `std.math` implementation. Inputs and results use arbitrary-precision signed `Int`; none of these operations silently wrap.

| Function | Contract | Cost |
|---|---|---|
| `gcd(a, b)` | Nonnegative greatest common divisor; `gcd(0, 0) = 0` | Euclidean algorithm, `O(log(min(|a|, |b|) + 1))` remainder steps |
| `lcm(a, b)` | Nonnegative least common multiple; zero if either input is zero; divides by `gcd` before multiplication | One GCD plus a multiplication |
| `min(a, b)` / `max(a, b)` | Exact signed integer order; on equality returns the left operand | Proportional to the compared integers' limb counts |
| `abs(a)` | Exact nonnegative magnitude | Proportional to the returned integer's limb count |
| `modular_add/sub/mul(a, b, m)` | Requires `m > 0`; returns the unique Euclidean residue in `[0, m)`; negative operands are accepted | Reduces operands before combining, then reduces the result |
| `modular_pow(base, exponent, m)` | Requires `m > 0` and `exponent >= 0`; returns `base^exponent mod m` in `[0, m)`; `0^0 mod m` is `1 mod m` | `O(log exponent)` modular multiplications |

Modulus zero or a negative modulus returns `ModularError::NonPositiveModulus`; a negative modular exponent returns `ModularError::NegativeExponent`. Modular exponentiation uses repeated squaring with arbitrary-precision intermediates, so it does not overflow fixed-width integers. Storage is proportional to the input and modulus bit lengths. Existing source `POW` and `ABS` instructions already provide general integer exponentiation and absolute value; the helpers here add reusable integer-library APIs.

## Standard-library boundary

These Rust implementations are available through the interpreter crate. Binding them as `std.math` functions depends on the later standard-module loading work; this document does not claim that imports are available yet.
