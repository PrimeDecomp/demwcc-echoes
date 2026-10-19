//! Fast path for candidate compiles ([`Mwcc::enable_fast`]): persistent compilers that parse a unit
//! context once and then compile each candidate after it in a few milliseconds.
//!
//! The compilers are the module's (GC 2.6 / 2.7, `mwdec_oracle::wasm`), in mwcc-wasm's
//! `PersistPool` ([`FastPool`]): routing by context, starts (from a context's third request on;
//! the request itself compiles normally), LRU snapshots, idle ones dropped, and the trust of
//! failures are its. A failure with a compiler diagnostic is reported without a normal compile (one
//! in 8 is still checked), marked [`crate::UNCONFIRMED_FAILURE`] so that callers which must not act
//! on it confirm it. An exact match is confirmed with a normal compile.
use mwdec_oracle::compile::Compiler;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

/// Persistent compilers per search worker (each a context snapshot and its instance, about 70 MB
/// together on CStateManager).
const PER_WORKER: usize = 2;

/// What a persistent compiler is started on: compiler + flags + context text + file name.
#[derive(Clone)]
pub(crate) struct Spec {
    pub key: u64,
    pub comp: Compiler,
    pub context: String,
    pub file_name: Option<String>,
}

/// Counters of a driver's fast path.
#[derive(Clone, Copy, Debug, Default)]
pub struct FastStats {
    /// Successful persistent compiles.
    pub compiles: u64,
    /// Persistent compiles that failed (redone normally).
    pub failed: u64,
    /// ... of which the normal compile succeeded (the fast path should never reject a candidate
    /// the compiler accepts).
    pub failed_normal_ok: u64,
    /// ... compiled normally again (the others were trusted as they were).
    pub failed_recompiled: u64,
    /// Requests compiled normally because no persistent compiler was ready.
    pub not_ready: u64,
    /// Persistent compilers started / failed to start.
    pub starts: u64,
    pub start_failures: u64,
    /// Total milliseconds in successful persistent compiles / in starts.
    pub compile_ms: f64,
    pub start_ms: f64,
    /// Exact matches re-checked with a normal compile, and how many of them disagreed.
    pub confirms: u64,
    pub confirm_mismatches: u64,
    /// `MWDEC_PERSIST_VERIFY`: fast results compared with a normal compile, and disagreements.
    pub verified: u64,
    pub verify_mismatches: u64,
    /// Context snapshots alive now / at most so far.
    pub live: usize,
    pub live_max: usize,
}

impl FastStats {
    pub fn add(&mut self, o: &FastStats) {
        self.compiles += o.compiles;
        self.failed += o.failed;
        self.failed_normal_ok += o.failed_normal_ok;
        self.failed_recompiled += o.failed_recompiled;
        self.not_ready += o.not_ready;
        self.starts += o.starts;
        self.start_failures += o.start_failures;
        self.compile_ms += o.compile_ms;
        self.start_ms += o.start_ms;
        self.confirms += o.confirms;
        self.confirm_mismatches += o.confirm_mismatches;
        self.verified += o.verified;
        self.verify_mismatches += o.verify_mismatches;
        self.live += o.live;
        self.live_max += o.live_max;
    }

    /// Fast compiles and normal compiles that disagreed (should be zero).
    pub fn mismatches(&self) -> u64 {
        self.failed_normal_ok + self.confirm_mismatches + self.verify_mismatches
    }

    /// One-line summary.
    pub fn line(&self) -> String {
        let avg = if self.compiles > 0 { self.compile_ms / self.compiles as f64 } else { 0.0 };
        let avg_start = if self.starts > 0 { self.start_ms / self.starts as f64 } else { 0.0 };
        format!(
            "fast path: {} compiles ({avg:.1} ms avg), {} failed ({} compiled normally again, {} accepted by a normal compile), {} not ready; \
             {} starts ({avg_start:.0} ms avg, {} failed), max {} snapshots; {} exact confirmations ({} disagreed), \
             {} verified ({} disagreed)",
            self.compiles,
            self.failed,
            self.failed_recompiled,
            self.failed_normal_ok,
            self.not_ready,
            self.starts,
            self.start_failures,
            self.live_max,
            self.confirms,
            self.confirm_mismatches,
            self.verified,
            self.verify_mismatches
        )
    }
}

/// The fast path of one driver: mwcc-wasm's persistent pool, and the contexts it has been given.
pub(crate) struct FastPool {
    pub pool: mwcc_wasm::PersistPool,
    contexts: Mutex<HashMap<u64, Arc<mwcc_wasm::pool::Context>>>,
    confirms: AtomicU64,
    confirm_mismatches: AtomicU64,
    verified: AtomicU64,
    verify_mismatches: AtomicU64,
}

impl FastPool {
    /// For `workers` search workers: [`PER_WORKER`] persistent compilers each.
    pub fn new(workers: usize) -> FastPool {
        FastPool {
            pool: mwcc_wasm::PersistPool::new(mwcc_wasm::PoolOptions {
                max_live: workers.max(1) * PER_WORKER,
                ..mwcc_wasm::PoolOptions::default()
            }),
            contexts: Mutex::new(HashMap::new()),
            confirms: AtomicU64::new(0),
            confirm_mismatches: AtomicU64::new(0),
            verified: AtomicU64::new(0),
            verify_mismatches: AtomicU64::new(0),
        }
    }

    /// The pool's context for `spec` (built once; bounded).
    pub fn context(&self, spec: &Spec, loaded: Arc<mwcc_wasm::Loaded>) -> std::io::Result<Arc<mwcc_wasm::pool::Context>> {
        let mut contexts = self.contexts.lock().unwrap();
        if let Some(c) = contexts.get(&spec.key) {
            return Ok(c.clone());
        }
        if contexts.len() >= 64 {
            contexts.clear();
        }
        // (the source is only in the overlay; its directory is the compiler's)
        let directory = std::path::absolute(&spec.comp.work)?;
        std::fs::create_dir_all(&directory)?;
        let ext = if spec.comp.profile.is_c() { "c" } else { "cpp" };
        let source = directory.join("wp").join(spec.file_name.clone().unwrap_or_else(|| format!("context.{ext}")));
        let output = directory.join("wp.o").to_string_lossy().into_owned();
        let mut args = spec.comp.flags();
        args.extend(["-c".into(), source.to_string_lossy().into_owned(), "-o".into(), output.clone()]);
        let exe = spec.comp.exe();
        let context = Arc::new(mwcc_wasm::pool::Context {
            module: loaded,
            args,
            output,
            directory,
            environment: std::env::vars().collect(),
            program: std::path::absolute(&exe).unwrap_or(exe).to_string_lossy().into_owned(),
            source,
            text: spec.context.clone(),
            overlay: mwcc_wasm::Overlay::new(),
            transcode_sjis: false,
        });
        contexts.insert(spec.key, context.clone());
        Ok(context)
    }

    pub fn note_confirm(&self, agreed: bool) {
        self.confirms.fetch_add(1, Relaxed);
        if !agreed {
            self.confirm_mismatches.fetch_add(1, Relaxed);
        }
    }
    pub fn note_verify(&self, agreed: bool) {
        self.verified.fetch_add(1, Relaxed);
        if !agreed {
            self.verify_mismatches.fetch_add(1, Relaxed);
        }
    }

    /// The pool's counters as the fast path's.
    pub fn stats(&self) -> FastStats {
        let p = self.pool.stats();
        FastStats {
            compiles: p.fast - p.trusted_failures,
            failed: p.trusted_failures + p.rechecked,
            failed_normal_ok: p.mismatches,
            failed_recompiled: p.rechecked,
            not_ready: p.not_ready,
            starts: p.starts,
            start_failures: p.start_failures,
            compile_ms: p.fast_ms,
            start_ms: p.start_ms,
            confirms: self.confirms.load(Relaxed),
            confirm_mismatches: self.confirm_mismatches.load(Relaxed),
            verified: self.verified.load(Relaxed),
            verify_mismatches: self.verify_mismatches.load(Relaxed),
            live: p.live,
            live_max: p.live_max,
        }
    }
}
