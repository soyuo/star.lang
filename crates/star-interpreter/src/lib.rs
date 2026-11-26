//! Static semantic validation for parsed Star programs.
//!
//! Runtime evaluation remains for a later roadmap item; parsed programs can be
//! checked through [`semantic::analyze`] before then.

pub mod semantic;
