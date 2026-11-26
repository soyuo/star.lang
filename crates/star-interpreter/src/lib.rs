//! Static semantic validation and reference execution for parsed Star programs.
//!
//! Use [`semantic::analyze`] for static diagnostics and [`runtime::Interpreter`]
//! to execute the currently supported reference-runtime slice.

pub use num_bigint::BigInt;

pub mod gc;
pub mod runtime;
pub mod semantic;
pub mod value;
