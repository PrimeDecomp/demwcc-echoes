//! The compiler run in process as WebAssembly instead of as `mwcceppc.exe`: the GC 2.6 / 2.7 compilers that
//! mwcc-tools builds from gc2's sources (`build/GC_2_7/wasm/mwcceppc.wasm`), run by its `mwcc-wasm` crate. No
//! process per compile, no `taskkill`, and the same objects as the executable.
//!
//! - [`run`] runs an executable's module when it has one (`None` otherwise: run the executable).
//! - [`Persistent`] parses a context once and compiles many candidates after it, in the compiler built for that
//!   (`persist.wasm`): the fast path's persistent compilers ([`crate::persist::compile_cached`]).
//! - [`run_oracle`] runs the compiler with mwcc-tools' trace hooks (`oracle.wasm`): the tracer
//!   ([`crate::oracle_trace`]).
//! - The modules are found in the mwcc-tools checkout `MWDEC_MWCC_TOOLS`, by default the one this crate was built
//!   against (next to this checkout). `MWDEC_WASM=0` turns it off.
//! - Each module is loaded once per process, from the compiled artifact the first process to load it wrote under
//!   the work base (`wasm-cache/GC_2_7/<module>/`): compiling it takes seconds, and the eval's draft servers
//!   restart. Each run instantiates it afresh (the compiler keeps its state in globals) on a thread with room for
//!   the compiler's recursion.
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
struct Loaded {
    compiler: mwcc_wasm::Compiler,
    version_info: Vec<u8>,
}

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

/// The module `name` (`mwcceppc`, `persist`, `oracle`) of the compiler `exe` (`.../GC/2.7/mwcceppc.exe`), if mwcc-tools
/// builds one for its version.
fn module_named(exe: &Path, name: &str) -> Option<PathBuf> {
    let file = exe.file_name()?.to_str()?;
    let version = exe.parent()?.file_name()?.to_str()?;
    let family = exe.parent()?.parent()?.file_name()?.to_str()?;
    if !file.eq_ignore_ascii_case("mwcceppc.exe") || family != "GC" || !matches!(version, "2.6" | "2.7") {
        return None;
    }
    Some(mwcc_tools().join(format!("build/GC_{}/wasm/{name}.wasm", version.replace('.', "_"))))
}

/// The module of the compiler `exe`.
pub fn module_for(exe: &Path) -> Option<PathBuf> {
    module_named(exe, "mwcceppc")
}

/// Whether compiles of `exe` run in process.
pub fn available(exe: &Path) -> bool {
    !disabled_by_env() && loaded(exe).is_some()
}

/// Whether persistent compiles of `exe` run in process ([`Persistent`]).
pub fn persistent_available(exe: &Path) -> bool {
    !disabled_by_env() && module_named(exe, "persist").and_then(|m| loaded_module(exe, m)).is_some()
}

fn loaded(exe: &Path) -> Option<Arc<Loaded>> {
    loaded_module(exe, module_for(exe)?)
}

fn loaded_module(exe: &Path, module: PathBuf) -> Option<Arc<Loaded>> {
    static LOADED: OnceLock<Mutex<HashMap<PathBuf, Option<Arc<Loaded>>>>> = OnceLock::new();
    let mut map = LOADED.get_or_init(Default::default).lock().unwrap();
    map.entry(module.clone())
        .or_insert_with(|| match load(exe, &module) {
            Ok(l) => Some(Arc::new(l)),
            Err(e) => {
                // (the module is optional: say once why it is not used)
                if module.exists() {
                    eprintln!("mwdec: running {} (the module {} did not load: {e:#})", exe.display(), module.display());
                }
                None
            }
        })
        .clone()
}

fn load(exe: &Path, module: &Path) -> Result<Loaded> {
    let _p = mwdec_core::prof::span("mwcc.wasm_load");
    let wasm = std::fs::read(module)?;
    // (a directory per version and module: writing a module's artifact removes the earlier builds')
    let version = module.parent().and_then(Path::parent).and_then(Path::file_name).unwrap_or_default();
    let cache = mwdec_core::paths::work_dir("wasm-cache").join(version).join(module.file_stem().unwrap_or_default());
    let compiler = mwcc_wasm::Compiler::new_cached(&wasm, &cache).map_err(|e| anyhow!("{e:?}"))?;
    let version_info = version_info(&std::fs::read(exe)?).unwrap_or_default();
    Ok(Loaded { compiler, version_info })
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
    let Some(module) = module_named(exe, "oracle") else { return false };
    let traces = *TRACES.get_or_init(Default::default).lock().unwrap().entry(module.clone()).or_insert_with(|| {
        std::fs::read(&module).is_ok_and(|m| m.windows(12).any(|w| w == b"ph.hit rule="))
    });
    traces && loaded_module(exe, module).is_some()
}

/// Run the oracle module of `exe` with the traces `traces` on (`MWCC_ORACLE_*` variables and their values); `None`:
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
    let loaded = loaded_module(exe, module_named(exe, "oracle")?)?;
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
    // (a thread of its own, with room for the compiler's recursion)
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn_scoped(s, || {
                loaded.compiler.run(&mwcc_wasm::Request {
                    args,
                    directory: &directory,
                    environment,
                    program: &program,
                    version_info: &loaded.version_info,
                    timeout: Some(timeout),
                    cancel: None,
                    trace,
                    overlay: None,
                    transcode_sjis: false,
                })
            })
            .map_err(|e| anyhow!("starting the compiler's thread: {e}"))?
            .join()
            .map_err(|_| anyhow!("the in-process compiler panicked"))?
            .map_err(|e| anyhow!("{e:?}"))
    })
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
        let loaded = loaded_module(exe, module_named(exe, "persist")?)?;
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

/// The VS_VERSIONINFO resource of the PE image `image` (what GetFileVersionInfoA gives): the driver reads its own.
pub fn version_info(image: &[u8]) -> Option<Vec<u8>> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(image.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(image.get(at..at + 4)?.try_into().ok()?));
    let pe = u32_at(0x3c)? as usize;
    if image.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let sections = u16_at(pe + 6)? as usize;
    let optional = pe + 24;
    let optional_size = u16_at(pe + 20)? as usize;
    // (the resource directory: data directory 2 of a PE32 optional header)
    let resources = u32_at(optional + 96 + 2 * 8)?;
    let table = optional + optional_size;
    let offset_of = |rva: u32| -> Option<usize> {
        (0..sections).find_map(|i| {
            let s = table + 40 * i;
            let (size, at, raw) = (u32_at(s + 8)?, u32_at(s + 12)?, u32_at(s + 20)?);
            (rva >= at && rva < at + size.max(u32_at(s + 16)?)).then_some((rva - at + raw) as usize)
        })
    };
    let base = offset_of(resources)?;
    // The first entry of a directory level whose id is `id` (any id when `None`): (is a directory, offset).
    let entry = |directory: usize, id: Option<u32>| -> Option<(bool, usize)> {
        let count = u16_at(directory + 12)? as usize + u16_at(directory + 14)? as usize;
        (0..count).find_map(|i| {
            let e = directory + 16 + 8 * i;
            let (name, target) = (u32_at(e)?, u32_at(e + 4)?);
            (id.is_none_or(|id| name == id)).then_some((target & 0x8000_0000 != 0, base + (target & 0x7fff_ffff) as usize))
        })
    };
    // type RT_VERSION (16) / its first name / its first language / the data entry
    let (true, names) = entry(base, Some(16))? else { return None };
    let (true, languages) = entry(names, None)? else { return None };
    let (false, data) = entry(languages, None)? else { return None };
    let at = offset_of(u32_at(data)?)?;
    Some(image.get(at..at + u32_at(data + 4)? as usize)?.to_vec())
}
