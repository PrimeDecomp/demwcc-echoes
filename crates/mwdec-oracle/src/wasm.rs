//! The compiler run in process as WebAssembly instead of as `mwcceppc.exe`: the GC 2.6 / 2.7 compiler that
//! mwcc-tools builds from gc2's sources (`build/GC_2_7/wasm/mwcc.wasm`), run by its `mwcc-wasm` crate. No process
//! per compile, no `taskkill`, and the same objects as the executable.
//!
//! - One module does everything: [`run`] compiles, [`run_oracle`] traces (the oracle's hooks are in it, and record
//!   only when a run asks), and a source with mwcc-wasm's persist marker is a persistent compile (the fast path's
//!   `mwcc_wasm::PersistPool`).
//! - GC 2.6 and 2.7 always run as their module: [`loaded`] is an error, with the command that builds it, when the
//!   module is missing or does not load. Other compilers (the SDK's GC 1.x) run as processes.
//! - The module is found in an mwcc-tools checkout (https://github.com/PrimeDecomp/mwcc-tools) that has it, from a
//!   release's `mwcc-wasm-GC_2_7-*.zip` unpacked into it or built there: `MWDEC_MWCC_TOOLS`, else `MWCC_ORACLE_ROOT`
//!   (the kits' name for it), else `mwcc-tools` next to this checkout. The crates come from that repository's Git
//!   (Cargo.toml), so no checkout is needed to build.
//! - mwcc-wasm's registry loads it once per process, from the compiled artifact its build writes beside it
//!   (`mwcc.cache/`), with the version information beside it (`mwcc.version`). Each run instantiates it afresh (the
//!   compiler keeps its state in globals) on a thread with room for the compiler's recursion.
use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// One compiler run.
pub struct Run {
    /// The status the compiler exited with.
    pub exit_code: i32,
    /// stdout and stderr, in the order written.
    pub console: String,
    pub timed_out: bool,
    /// Stopped by its `cancel` flag.
    pub cancelled: bool,
}

/// A module ready to run, with the version information of the executable it stands for.
pub type Loaded = mwcc_wasm::Loaded;

/// The mwcc-tools checkout that has the modules.
fn mwcc_tools() -> PathBuf {
    match std::env::var_os("MWDEC_MWCC_TOOLS").or_else(|| std::env::var_os("MWCC_ORACLE_ROOT")) {
        Some(p) => PathBuf::from(p),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../mwcc-tools"),
    }
}

/// The version directory (`GC_2_7`) of a compiler `exe` (`.../GC/2.7/mwcceppc.exe`) that mwcc-tools builds a
/// module for.
fn module_version(exe: &Path) -> Option<String> {
    let file = exe.file_name()?.to_str()?;
    let version = exe.parent()?.file_name()?.to_str()?;
    let family = exe.parent()?.parent()?.file_name()?.to_str()?;
    if !file.eq_ignore_ascii_case("mwcceppc.exe") || family != "GC" || !matches!(version, "2.6" | "2.7") {
        return None;
    }
    Some(format!("GC_{}", version.replace('.', "_")))
}

/// The module of the compiler `exe`, if mwcc-tools builds one for its version (it then never runs as a process).
pub fn module_for(exe: &Path) -> Option<PathBuf> {
    module_version(exe).map(|v| mwcc_tools().join(format!("build/{v}/wasm/mwcc.wasm")))
}

/// How to build the module of `exe`.
fn build_hint(exe: &Path) -> String {
    let version = module_version(exe).unwrap_or_default();
    format!(
        "unpack an mwcc-tools release's mwcc-wasm-{version}-*.zip into the mwcc-tools checkout {} (GC_2_7; https://github.com/PrimeDecomp/mwcc-tools/releases), or build it there with `ninja build/{version}/wasm/mwcc.wasm`; MWDEC_MWCC_TOOLS (else MWCC_ORACLE_ROOT) names another checkout",
        mwcc_tools().display()
    )
}

/// The module of `exe`, loaded. `Ok(None)`: its version has none (it runs as a process). Err: it needs one that is
/// missing or did not load (said once on stderr too, since callers may count it as a failed compile).
pub fn loaded(exe: &Path) -> Result<Option<Arc<Loaded>>> {
    static FAILED: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();
    let Some(module) = module_for(exe) else { return Ok(None) };
    if let Some(e) = FAILED.get_or_init(Default::default).lock().unwrap().get(&module) {
        bail!("{e}");
    }
    let error = if !module.exists() {
        format!("{} runs as mwcc-tools' module {}, which is missing: {}", exe.display(), module.display(), build_hint(exe))
    } else {
        let _p = mwdec_core::prof::span("mwcc.wasm_load");
        match mwcc_wasm::load(&module) {
            Ok(loaded) => return Ok(Some(loaded)),
            Err(e) => format!("{} runs as mwcc-tools' module {}, which did not load ({e:#}): rebuild it; {}", exe.display(), module.display(), build_hint(exe)),
        }
    };
    eprintln!("mwdec: {error}");
    FAILED.get_or_init(Default::default).lock().unwrap().insert(module, error.clone());
    bail!("{error}")
}

/// Run the module `loaded` of the compiler `exe` with `args` in `directory`, stopped once `cancel` is set. Err: the
/// compiler trapped (a crash).
pub fn run(
    loaded: &Loaded,
    exe: &Path,
    args: &[String],
    directory: &Path,
    timeout: Duration,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<Run> {
    let environment: Vec<(String, String)> = std::env::vars().collect();
    execute(loaded, exe, args, directory, timeout, &environment, false, cancel).map(|o| Run {
        exit_code: o.exit_code,
        console: o.console.iter().map(|b| char::from(*b)).collect(),
        timed_out: o.timed_out,
        cancelled: o.cancelled,
    })
}

/// One traced run of the module: the compiler with mwcc-tools' trace hooks.
pub struct Traced {
    pub exit_code: i32,
    /// The compiler's messages (its stdout).
    pub messages: String,
    /// The trace records, one a line (its stderr, with the driver's few lines of its own).
    pub trace: String,
    pub timed_out: bool,
}

/// Run the module of `exe` tracing, with the traces `traces` on (`MWCC_ORACLE_*` variables and their values). The
/// module keeps the records in memory (`mwcc_wasm::Outcome::trace`). Err: `exe` has no module (only GC 2.6 and 2.7
/// trace), the module is missing or older than the records the tracer reads (the last of them is the peephole's,
/// whose format is in the module's data), or the compiler trapped.
pub fn run_oracle(exe: &Path, args: &[String], directory: &Path, timeout: Duration, traces: &[(String, String)]) -> Result<Traced> {
    static TRACES: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let Some(loaded) = loaded(exe)? else { bail!("tracer: {} has no module (only GC 2.6 and 2.7 trace)", exe.display()) };
    let module = module_for(exe).expect("a module");
    let current = *TRACES.get_or_init(Default::default).lock().unwrap().entry(module.clone()).or_insert_with(|| {
        std::fs::read(&module).is_ok_and(|m| m.windows(12).any(|w| w == b"ph.hit rule="))
    });
    if !current {
        bail!("tracer: the module {} predates the peephole records: {}", module.display(), build_hint(exe));
    }
    let mut environment: Vec<(String, String)> = std::env::vars().filter(|(k, _)| !k.starts_with("MWCC_ORACLE_")).collect();
    environment.extend(traces.iter().cloned());
    execute(&loaded, exe, args, directory, timeout, &environment, true, None).map(|o| Traced {
        exit_code: o.exit_code,
        messages: o.stdout.iter().map(|b| char::from(*b)).collect(),
        trace: String::from_utf8_lossy(&o.trace).into_owned(),
        timed_out: o.timed_out || o.cancelled,
    })
}

fn execute(
    loaded: &Loaded,
    exe: &Path,
    args: &[String],
    directory: &Path,
    timeout: Duration,
    environment: &[(String, String)],
    trace: bool,
    cancel: Option<Arc<AtomicBool>>,
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
                // (the registry's run polls the closure below instead)
                cancel: None,
                trace,
                overlay: None,
                transcode_sjis: false,
                record_reads: false,
            },
            &|| cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)),
        )
        .map_err(|e| anyhow!("{e:?}"))
}
