//! The compiler run in process as WebAssembly instead of as `mwcceppc.exe`: the GC 2.6 / 2.7 compiler that
//! mwcc-tools builds from gc2's sources (`build/GC_2_7/wasm/mwcc.wasm`), run by its `mwcc-wasm` crate. No process
//! per compile, no `taskkill`, and the same objects as the executable.
//!
//! - One module does everything: [`run`] compiles, [`run_oracle`] traces (the oracle's hooks are in it, and record
//!   only when a run asks), and a source with mwcc-wasm's persist marker is a persistent compile ([`Persistent`],
//!   and the fast path's `mwcc_wasm::PersistPool`).
//! - The module is found in the mwcc-tools checkout `MWDEC_MWCC_TOOLS`, by default the one this crate was built
//!   against (next to this checkout). `MWDEC_WASM=0` turns it off.
//! - mwcc-wasm's registry loads it once per process, from the compiled artifact its build writes beside it
//!   (`mwcc.cache/`), with the version information beside it (`mwcc.version`). Each run instantiates it afresh (the
//!   compiler keeps its state in globals) on a thread with room for the compiler's recursion.
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// One compiler run.
pub struct Run {
    /// The status the compiler exited with.
    pub exit_code: i32,
    /// stdout and stderr, in the order written.
    pub console: String,
    pub timed_out: bool,
}

/// A module ready to run, with the version information of the executable it stands for.
pub type Loaded = mwcc_wasm::Loaded;

/// `MWDEC_WASM=0` (or `off` / `no` / `false`) runs the executables.
pub fn disabled_by_env() -> bool {
    std::env::var("MWDEC_WASM").is_ok_and(|v| matches!(v.trim(), "0" | "off" | "no" | "false"))
}

/// The mwcc-tools checkout the modules are built in.
fn mwcc_tools() -> PathBuf {
    match std::env::var_os("MWDEC_MWCC_TOOLS") {
        Some(p) => PathBuf::from(p),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../mwcc"),
    }
}

/// The module of the compiler `exe` (`.../GC/2.7/mwcceppc.exe`), if mwcc-tools builds one for its version.
pub fn module_for(exe: &Path) -> Option<PathBuf> {
    let file = exe.file_name()?.to_str()?;
    let version = exe.parent()?.file_name()?.to_str()?;
    let family = exe.parent()?.parent()?.file_name()?.to_str()?;
    if !file.eq_ignore_ascii_case("mwcceppc.exe") || family != "GC" || !matches!(version, "2.6" | "2.7") {
        return None;
    }
    Some(mwcc_tools().join(format!("build/GC_{}/wasm/mwcc.wasm", version.replace('.', "_"))))
}

/// Whether compiles of `exe` run in process.
pub fn available(exe: &Path) -> bool {
    !disabled_by_env() && loaded(exe).is_some()
}

/// Whether persistent compiles of `exe` run in process ([`Persistent`], `mwcc_wasm::PersistPool`).
pub fn persistent_available(exe: &Path) -> bool {
    available(exe)
}

/// The module of `exe`, loaded (`None`: it has none, or it did not load, which is said once).
pub fn loaded(exe: &Path) -> Option<Arc<Loaded>> {
    static FAILED: OnceLock<Mutex<HashMap<PathBuf, ()>>> = OnceLock::new();
    let module = module_for(exe)?;
    if !module.exists() {
        return None;
    }
    let _p = mwdec_core::prof::span("mwcc.wasm_load");
    match mwcc_wasm::load(&module) {
        Ok(loaded) => Some(loaded),
        Err(e) => {
            // (the module is optional: say once why it is not used)
            if FAILED.get_or_init(Default::default).lock().unwrap().insert(module.clone(), ()).is_none() {
                eprintln!("mwdec: running {} (the module {} did not load: {e:#})", exe.display(), module.display());
            }
            None
        }
    }
}

/// Run the compiler `exe` with `args` in `directory` as its module, if it has one; `None`: run `exe`.
/// Err: the compiler trapped (a crash).
pub fn run(exe: &Path, args: &[String], directory: &Path, timeout: Duration) -> Option<Result<Run>> {
    if disabled_by_env() {
        return None;
    }
    let loaded = loaded(exe)?;
    let environment: Vec<(String, String)> = std::env::vars().collect();
    Some(execute(&loaded, exe, args, directory, timeout, &environment, false).map(|o| Run {
        exit_code: o.exit_code,
        console: o.console.iter().map(|b| char::from(*b)).collect(),
        timed_out: o.timed_out || o.cancelled,
    }))
}

/// One run of the oracle module: the compiler with mwcc-tools' trace hooks linked in (`oracle.wasm`).
pub struct Traced {
    pub exit_code: i32,
    /// The compiler's messages (its stdout).
    pub messages: String,
    /// The trace records, one a line (its stderr, with the driver's few lines of its own).
    pub trace: String,
    pub timed_out: bool,
}

/// Whether `exe` has an oracle module with every record the tracer reads ([`crate::oracle_trace`]): an older one
/// lacks the last of them, the peephole's (its format is in the module's data).
pub fn oracle_available(exe: &Path) -> bool {
    static TRACES: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    if disabled_by_env() {
        return false;
    }
    let Some(module) = module_for(exe) else { return false };
    let traces = *TRACES.get_or_init(Default::default).lock().unwrap().entry(module.clone()).or_insert_with(|| {
        std::fs::read(&module).is_ok_and(|m| m.windows(12).any(|w| w == b"ph.hit rule="))
    });
    traces && loaded(exe).is_some()
}

/// Run the module of `exe` tracing, with the traces `traces` on (`MWCC_ORACLE_*` variables and their values); `None`:
/// it has none. The module keeps the records in memory (`mwcc_wasm::Outcome::trace`).
pub fn run_oracle(
    exe: &Path,
    args: &[String],
    directory: &Path,
    timeout: Duration,
    traces: &[(&str, String)],
) -> Option<Result<Traced>> {
    if disabled_by_env() {
        return None;
    }
    let loaded = loaded(exe)?;
    let mut environment: Vec<(String, String)> =
        std::env::vars().filter(|(k, _)| !k.starts_with("MWCC_ORACLE_")).collect();
    environment.extend(traces.iter().map(|(k, v)| (k.to_string(), v.clone())));
    Some(execute(&loaded, exe, args, directory, timeout, &environment, true).map(|o| Traced {
        exit_code: o.exit_code,
        messages: o.stdout.iter().map(|b| char::from(*b)).collect(),
        trace: String::from_utf8_lossy(&o.trace).into_owned(),
        timed_out: o.timed_out || o.cancelled,
    }))
}

fn execute(
    loaded: &Loaded,
    exe: &Path,
    args: &[String],
    directory: &Path,
    timeout: Duration,
    environment: &[(String, String)],
    trace: bool,
) -> Result<mwcc_wasm::Outcome> {
    let program = std::path::absolute(exe).unwrap_or_else(|_| exe.to_path_buf()).to_string_lossy().into_owned();
    let directory = std::path::absolute(directory).unwrap_or_else(|_| directory.to_path_buf());
    // (on a thread of its own, with room for the compiler's recursion: the registry's run)
    loaded
        .run(
            &mwcc_wasm::Request {
                args,
                directory: &directory,
                environment,
                program: &program,
                version_info: &[],
                timeout: Some(timeout),
                cancel: None,
                trace,
                overlay: None,
                transcode_sjis: false,
                record_reads: false,
            },
            &|| false,
        )
        .map_err(|e| anyhow!("{e:?}"))
}

/// A persistent compile: the compiler stopped after a context, compiling each candidate after it from a snapshot of
/// that moment (`mwcc_wasm::Persistent`). Its source lives in a directory of its own, removed with it.
pub struct Persistent {
    /// (ended before its directory is removed)
    inner: Option<mwcc_wasm::Persistent>,
    directory: PathBuf,
}

/// A persistent candidate's result.
pub struct Candidate {
    /// The status the compiler exits with: 0 compiled, 1 failed, 2 too many errors.
    pub exit_code: i32,
    /// The object, when it compiled.
    pub object: Option<Vec<u8>>,
    /// Its messages.
    pub console: String,
}

impl Persistent {
    /// Start `exe`'s persistent compile of `context` with `flags` in `work` (the source is named `file_name`, else a
    /// scratch name with `extension`), for candidates of up to `capacity` bytes; `None`: `exe` has no persistent
    /// module. Returns once the context is parsed.
    pub fn start(exe: &Path, flags: &[String], work: &Path, context: &str, capacity: usize, file_name: Option<&str>, extension: &str) -> Option<Result<Persistent>> {
        if disabled_by_env() {
            return None;
        }
        let loaded = loaded(exe)?;
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let stem = format!("wp{}_{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let directory = work.join(&stem);
        let start = || -> Result<Persistent> {
            std::fs::create_dir_all(&directory)?;
            let source = directory.join(file_name.map_or_else(|| format!("{stem}.{extension}"), str::to_owned));
            std::fs::write(&source, mwcc_wasm::persistent_source(context, capacity))?;
            let mut args = flags.to_vec();
            args.extend(["-c".into(), source.to_string_lossy().into_owned(), "-o".into(), directory.join(format!("{stem}.o")).to_string_lossy().into_owned()]);
            let program = std::path::absolute(exe).unwrap_or_else(|_| exe.to_path_buf()).to_string_lossy().into_owned();
            let environment: Vec<(String, String)> = std::env::vars().collect();
            let inner = loaded
                .compiler
                .persistent(
                    &mwcc_wasm::Request {
                        args: &args,
                        directory: &std::path::absolute(work).unwrap_or_else(|_| work.to_path_buf()),
                        environment: &environment,
                        program: &program,
                        version_info: &loaded.version_info,
                        timeout: Some(START_TIMEOUT),
                        cancel: None,
                        trace: false,
                        overlay: None,
                        transcode_sjis: false,
                        record_reads: false,
                    },
                    capacity,
                )
                .map_err(|e| anyhow!("{e:?}"))?;
            Ok(Persistent { inner: Some(inner), directory: directory.clone() })
        };
        let started = start();
        if started.is_err() {
            let _ = std::fs::remove_dir_all(&directory);
        }
        Some(started)
    }

    /// Compile `candidate` (the text after the context). Err: it did not run to its end (a timeout or a trap: the
    /// compiler is then unusable, [`alive`](Self::alive)), or it does not fit.
    pub fn compile(&mut self, candidate: &str, timeout: Duration) -> Result<Candidate> {
        let c = self.inner.as_mut().expect("running").compile(candidate.as_bytes(), Some(timeout)).map_err(|e| anyhow!("{e:?}"))?;
        Ok(Candidate { exit_code: c.exit_code, object: c.object, console: c.console.iter().map(|b| char::from(*b)).collect() })
    }

    /// Whether it can still compile.
    pub fn alive(&self) -> bool {
        self.inner.as_ref().is_some_and(mwcc_wasm::Persistent::alive)
    }
}

impl Drop for Persistent {
    fn drop(&mut self) {
        drop(self.inner.take());
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// How long a persistent compile may take to parse its context.
const START_TIMEOUT: Duration = Duration::from_secs(300);
