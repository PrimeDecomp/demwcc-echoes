//! The compiler run in process as WebAssembly instead of as `mwcceppc.exe`: the GC 2.6 / 2.7 compilers that
//! mwcc-tools builds from gc2's sources (`build/GC_2_7/wasm/mwcceppc.wasm`), run by its `mwcc-wasm` crate. No
//! process per compile, no `taskkill`, and the same objects as the executable.
//!
//! - [`run`] runs an executable's module when it has one (`None` otherwise: run the executable).
//! - The modules are found in the mwcc-tools checkout `MWDEC_MWCC_TOOLS`, by default the one this crate was built
//!   against (next to this checkout). `MWDEC_WASM=0` turns it off.
//! - Each module is loaded once per process, from the compiled artifact the first process to load it wrote under
//!   the work base (`wasm-cache/GC_2_7/`): compiling it takes seconds, and the eval's draft servers restart. Each run
//!   instantiates it afresh (the compiler keeps its state in globals) on a thread with room for the compiler's
//!   recursion.
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

/// The module of the compiler `exe` (`.../GC/2.7/mwcceppc.exe`), if mwcc-tools builds one for its version.
pub fn module_for(exe: &Path) -> Option<PathBuf> {
    let name = exe.file_name()?.to_str()?;
    let version = exe.parent()?.file_name()?.to_str()?;
    let family = exe.parent()?.parent()?.file_name()?.to_str()?;
    if !name.eq_ignore_ascii_case("mwcceppc.exe") || family != "GC" || !matches!(version, "2.6" | "2.7") {
        return None;
    }
    Some(mwcc_tools().join(format!("build/GC_{}/wasm/mwcceppc.wasm", version.replace('.', "_"))))
}

/// Whether compiles of `exe` run in process.
pub fn available(exe: &Path) -> bool {
    !disabled_by_env() && loaded(exe).is_some()
}

fn loaded(exe: &Path) -> Option<Arc<Loaded>> {
    static LOADED: OnceLock<Mutex<HashMap<PathBuf, Option<Arc<Loaded>>>>> = OnceLock::new();
    let module = module_for(exe)?;
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
    // (a directory per version: writing a module's artifact removes the earlier builds')
    let version = module.parent().and_then(Path::parent).and_then(Path::file_name).unwrap_or_default();
    let cache = mwdec_core::paths::work_dir("wasm-cache").join(version);
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
    let program = std::path::absolute(exe).unwrap_or_else(|_| exe.to_path_buf()).to_string_lossy().into_owned();
    let directory = std::path::absolute(directory).unwrap_or_else(|_| directory.to_path_buf());
    let environment: Vec<(String, String)> = std::env::vars().collect();
    // (a thread of its own, with room for the compiler's recursion)
    let outcome = std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn_scoped(s, || {
                loaded.compiler.run(&mwcc_wasm::Request {
                    args,
                    directory: &directory,
                    environment: &environment,
                    program: &program,
                    version_info: &loaded.version_info,
                    timeout: Some(timeout),
                    cancel: None,
                })
            })
            .map_err(|e| anyhow!("starting the compiler's thread: {e}"))?
            .join()
            .map_err(|_| anyhow!("the in-process compiler panicked"))?
            .map_err(|e| anyhow!("{e:?}"))
    });
    Some(outcome.map(|o| Run {
        exit_code: o.exit_code,
        console: o.console.iter().map(|b| char::from(*b)).collect(),
        timed_out: o.timed_out || o.cancelled,
    }))
}

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
