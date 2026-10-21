//! Real-compiler tracer: the GC 2.6 / 2.7 compiler with mwcc-tools' trace hooks runs in process
//! ([`crate::wasm::run_oracle`]) and its records are read (mwcc-trace's `records::parse`) into a [`Trace`]:
//!
//! - **inliner**: every `CInline_InlineFunctionCheck` decision (caller, callee, expansion level,
//!   estimated cost vs `inline_max_size`, result and the code path that decided it), and the order
//!   in which function bodies are expanded/generated (deferred inlining order);
//! - **colouring**: per function and register class, every interference-graph node: vreg number,
//!   variable name (params / named locals / FE temps), value kind, IG degree, assigned register,
//!   coalescing, and the exact colouring (pop) order;
//! - **PCode** (optional): the backend instruction stream with virtual registers at three stages.
//!
//! Diagnostic only: matching always uses the plain compiler.

use crate::compile::Compiler;
use anyhow::{bail, Context, Result};
use serde::Serialize;

// ------------------------------------------------------------------ results
pub use mwcc_trace::records::{
    ColorRound, CopyPropDecision, CopyRemoval, IgNode, InlineDecision, PcodeDump, PeepholeHit, Trace, VregKind,
};
/// What a trace reads (mwcc-trace's `Selection`).
pub use mwcc_trace::records::Selection as TraceOptions;

/// Trace one compile of `source` with the compiler's flag profile. Err: the compiler has no module
/// (only GC 2.6 and 2.7 trace), its module is missing or too old, or the compile failed.
pub fn trace_source(comp: &Compiler, source: &str, opts: &TraceOptions) -> Result<Trace> {
    let _p = mwdec_core::prof::span("tracer.oracle");
    let exe = comp.exe();
    std::fs::create_dir_all(&comp.work)?;
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stem = format!("tr{}_{}", std::process::id(), n);
    let ext = if comp.profile.is_c() { "c" } else { "cpp" };
    let src = comp.work.join(format!("{stem}.{ext}"));
    let obj = comp.work.join(format!("{stem}.o"));
    std::fs::write(&src, source)?;
    let mut args = comp.flags();
    args.extend(["-c".into(), src.to_string_lossy().into_owned(), "-o".into(), obj.to_string_lossy().into_owned()]);
    let log = comp.work.join(format!("{stem}.log"));
    let variables: Vec<(String, String)> = opts.trace_options(&log).environment().into_iter().collect();
    let run = crate::wasm::run_oracle(&exe, &args, &comp.work, std::time::Duration::from_secs(300), &variables);
    let _ = std::fs::remove_file(&src);
    let run = run.with_context(|| format!("tracing with {}", exe.display()))?;
    if run.timed_out {
        bail!("tracer: timeout waiting for the compiler");
    }
    let mut trace = mwcc_trace::records::parse(&run.trace, opts);
    if opts.iro {
        trace.iro = std::fs::read(&log).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    }
    let _ = std::fs::remove_file(&log);
    trace.object = std::fs::read(&obj).with_context(|| "tracer: compiler produced no object (compile error?)")?;
    let _ = std::fs::remove_file(&obj);
    Ok(trace)
}

// ------------------------------------------------------------------ targeted register fixes

#[derive(Clone, Debug, Serialize)]
pub struct RegisterFix {
    /// the candidate's value that sits in `from` but should be in `to`
    pub vreg: u16,
    pub name: Option<String>,
    pub kind: VregKind,
    pub from: u8,
    pub to: u8,
    /// the candidate value currently holding `to` (if any)
    pub holder: Option<(u16, Option<String>, VregKind)>,
    pub suggestion: String,
}

fn label(n: &IgNode) -> String {
    match &n.name {
        Some(s) => format!("`{s}` (v{})", n.vreg),
        None => format!("temp v{}", n.vreg),
    }
}

/// For a register-only diff: `moves` = (candidate register, target register) pairs for callee-saved
/// registers that disagree. Returns which candidate value is involved, who holds the wanted
/// register, and the source-level mutation that changes their colouring order (regalloc.md rules).
pub fn register_fixes(round: &ColorRound, moves: &[(u8, u8)]) -> Vec<RegisterFix> {
    let mut out = vec![];
    // the callee-saved value per register: the one coloured LAST into it overlaps the diff region
    // poorly; we take the first in colouring order (values sharing a register do not overlap).
    let by_reg = |r: u8| -> Vec<&IgNode> {
        let mut v: Vec<&IgNode> = round.nodes.iter().filter(|n| n.reg == Some(r) && n.coalesced_into.is_none() && n.degree > 0).collect();
        v.sort_by_key(|n| n.order.unwrap_or(usize::MAX));
        v
    };
    for &(from, to) in moves {
        if let Some(n) = by_reg(from).into_iter().next() {
            let holder = by_reg(to).into_iter().find(|h| h.neighbours.contains(&(n.vreg as i16)));
            let earlier = to > from; // higher register number = coloured earlier (r31 first)
            let s = if n.blocked {
                format!(
                    "{} is K-blocked (degree {} >= K): it is coloured before everything else; its register only changes \
                     if its degree drops below K (fewer overlapping temps) or other blocked values change",
                    label(n),
                    n.degree
                )
            } else if let Some(h) = holder {
                let (hn, hk) = (label(h), h.kind);
                match (n.kind, hk, earlier) {
                    (VregKind::Named, VregKind::Named, true) => format!("declare {} before {}", label(n), hn),
                    (VregKind::Named, VregKind::Named, false) => format!("declare {} after {}", label(n), hn),
                    (VregKind::Temp, VregKind::Temp, true) => format!("create {} after {} (evaluate it later; args run right to left)", label(n), hn),
                    (VregKind::Temp, VregKind::Temp, false) => format!("create {} before {}", label(n), hn),
                    (VregKind::Named, VregKind::Temp, true) => format!(
                        "make {} temp-class (all uses non-move: unname it or remove its plain register uses), or make {} named (give it a plain move-use)",
                        label(n),
                        hn
                    ),
                    (VregKind::Temp, VregKind::Named, false) => format!(
                        "make {} named (add a plain move-use, e.g. pass it as-is), or make {} temp-class",
                        label(n),
                        hn
                    ),
                    (VregKind::Param, _, _) | (_, VregKind::Param, _) => format!(
                        "params rank lowest in fixed order ({} vs {}); change which value is a param copy, \
                         or use `const` on a by-value param passed as a call argument (becomes an FE temp)",
                        label(n),
                        hn
                    ),
                    (VregKind::FeTemp, _, _) | (_, VregKind::FeTemp, _) => format!(
                        "{} vs {}: FE temps (inline-expansion locals, const-param copies) rank above named locals and below codegen temps; \
                         inline/out-of-line the helper or change the local into/out of an inlined body",
                        label(n),
                        hn
                    ),
                    _ => format!("{} and {} would need their class order reversed", label(n), hn),
                }
            } else {
                format!(
                    "{} gets r{} because the lowest free already-obtained register is reused; to reach r{}, the value holding it \
                     must overlap {} (or be coloured later)",
                    label(n),
                    from,
                    to,
                    label(n)
                )
            };
            out.push(RegisterFix {
                vreg: n.vreg,
                name: n.name.clone(),
                kind: n.kind,
                from,
                to,
                holder: holder.map(|h| (h.vreg, h.name.clone(), h.kind)),
                suggestion: s,
            });
        }
    }
    out
}
