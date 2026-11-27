//! Exact integer math primitives for Star's standard library.

use num_traits::{One, Signed, Zero};

use crate::BigInt;

/// An invalid input to modular arithmetic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModularError {
    /// Modular operations require a modulus greater than zero.
    NonPositiveModulus,
    /// Modular exponentiation requires an exponent greater than or equal to zero.
    NegativeExponent,
}

impl std::fmt::Display for ModularError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonPositiveModulus => formatter.write_str("modulus must be positive"),
            Self::NegativeExponent => formatter.write_str("modular exponent must be nonnegative"),
        }
    }
}

impl std::error::Error for ModularError {}

/// Returns the nonnegative greatest common divisor; `gcd(0, 0)` is zero.
pub fn gcd(left: &BigInt, right: &BigInt) -> BigInt {
    let mut left = left.abs();
    let mut right = right.abs();
    while !right.is_zero() {
        let remainder = &left % &right;
        left = right;
        right = remainder;
    }
    left
}

/// Returns the nonnegative least common multiple; it is zero if either input is zero.
pub fn lcm(left: &BigInt, right: &BigInt) -> BigInt {
    if left.is_zero() || right.is_zero() {
        return BigInt::zero();
    }
    ((left / gcd(left, right)) * right).abs()
}

/// Returns the smaller integer, choosing `left` when both values are equal.
pub fn min(left: BigInt, right: BigInt) -> BigInt {
    if left <= right {
        left
    } else {
        right
    }
}

/// Returns the larger integer, choosing `left` when both values are equal.
pub fn max(left: BigInt, right: BigInt) -> BigInt {
    if left >= right {
        left
    } else {
        right
    }
}

/// Returns the exact nonnegative magnitude of an integer.
pub fn abs(value: &BigInt) -> BigInt {
    value.abs()
}

/// Adds two integers modulo a positive modulus, returning a residue in `[0, modulus)`.
pub fn modular_add(
    left: &BigInt,
    right: &BigInt,
    modulus: &BigInt,
) -> Result<BigInt, ModularError> {
    validate_modulus(modulus)?;
    let left = residue(left, modulus);
    let right = residue(right, modulus);
    Ok(residue(&(left + right), modulus))
}

/// Subtracts two integers modulo a positive modulus, returning a residue in `[0, modulus)`.
pub fn modular_sub(
    left: &BigInt,
    right: &BigInt,
    modulus: &BigInt,
) -> Result<BigInt, ModularError> {
    validate_modulus(modulus)?;
    let left = residue(left, modulus);
    let right = residue(right, modulus);
    Ok(residue(&(left - right), modulus))
}

/// Multiplies two integers modulo a positive modulus, returning a residue in `[0, modulus)`.
pub fn modular_mul(
    left: &BigInt,
    right: &BigInt,
    modulus: &BigInt,
) -> Result<BigInt, ModularError> {
    validate_modulus(modulus)?;
    let left = residue(left, modulus);
    let right = residue(right, modulus);
    Ok(residue(&(left * right), modulus))
}

/// Computes `base^exponent mod modulus` with arbitrary-precision intermediates.
///
/// The modulus must be positive and the exponent nonnegative. The result is
/// always the unique residue in `[0, modulus)`.
pub fn modular_pow(
    base: &BigInt,
    exponent: &BigInt,
    modulus: &BigInt,
) -> Result<BigInt, ModularError> {
    validate_modulus(modulus)?;
    if exponent.is_negative() {
        return Err(ModularError::NegativeExponent);
    }

    let mut exponent = exponent.clone();
    let mut base = residue(base, modulus);
    let mut result = BigInt::one() % modulus;
    let two = BigInt::from(2u8);

    while !exponent.is_zero() {
        if !(&exponent % &two).is_zero() {
            result = residue(&(result * &base), modulus);
        }
        exponent /= &two;
        if !exponent.is_zero() {
            base = residue(&(&base * &base), modulus);
        }
    }

    Ok(result)
}

fn validate_modulus(modulus: &BigInt) -> Result<(), ModularError> {
    if modulus <= &BigInt::zero() {
        Err(ModularError::NonPositiveModulus)
    } else {
        Ok(())
    }
}

fn residue(value: &BigInt, modulus: &BigInt) -> BigInt {
    let remainder = value % modulus;
    if remainder.is_negative() {
        remainder + modulus
    } else {
        remainder
    }
}
