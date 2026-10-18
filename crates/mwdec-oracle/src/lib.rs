//! mwdec-oracle: experiment harness around the real MWCC compiler.
//!
//! - [`compile::Compiler`]: compile a source string with a project flag profile.
//! - [`asm`]: parse the ELF and print an annotated listing (relocs, literal values, labels).
//! - [`variants`]: experiment files with variants + machine-checked expectations.
//! - [`webs`]: recover callee-saved register webs (live ranges, interference) from object code.
//! - [`wasm`]: the compiler run in process as WebAssembly (GC 2.6 / 2.7), in place of the executable.
//! - [`regalloc`]: model of MWCC's register colouring (predict / inverse-solve priority orders).
pub mod advice;
pub mod asm;
pub mod compile;
pub mod explain;
pub mod flags;
pub mod hints;
pub mod iro;
pub mod persist;
pub mod regalloc;
pub mod sched;
pub mod schedcheck;
pub mod tracer;
pub mod variants;
pub mod wasm;
pub mod webs;
