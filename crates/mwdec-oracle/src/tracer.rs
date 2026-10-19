//! Real-compiler tracer: the GC 2.6 / 2.7 compiler with mwcc-tools' trace hooks runs in process
//! ([`crate::wasm::run_oracle`]) and its records are read ([`crate::oracle_trace`]) into a [`Trace`]:
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
#[derive(Clone, Debug, Serialize)]
pub struct InlineDecision {
    /// function whose body was being expanded
    pub caller: String,
    pub callee: String,
    /// expansion pass (cinline_level) of the caller
    pub pass: i16,
    /// depth budget passed to the size estimate
    pub level: Option<i32>,
    /// estimated cost of the callee's (expanded) body
    pub cost: Option<i32>,
    pub max: i32,
    pub inlined: bool,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum VregKind {
    Param,
    Named,
    FeTemp,
    Temp,
}

#[derive(Clone, Debug, Serialize)]
pub struct IgNode {
    pub vreg: u16,
    pub name: Option<String>,
    pub kind: VregKind,
    /// IG adjacency size (physical + virtual neighbours) before simplification
    pub degree: u16,
    /// final register number (0..31) or None if spilled/unresolved
    pub reg: Option<u8>,
    pub coalesced_into: Option<i16>,
    pub spilled: bool,
    pub neighbours: Vec<i16>,
    /// position in colouring order (0 = coloured first); None for coalesced nodes
    pub order: Option<usize>,
    /// pushed in a later simplify pass (degree >= K), i.e. coloured ahead of its vreg rank
    pub blocked: bool,
    /// spill cost computed before simplify (uses 2w, defs w; see `regalloc::spill_cost`)
    pub spill_cost: i32,
    /// rematerialisable (single location-independent definition: li/lis/addi rX,r1,N)
    pub remat: bool,
    /// current IG degree when simplify started (after coalescing); simplify compares this with K
    pub simplify_degree: u16,
}

#[derive(Clone, Debug, Serialize)]
pub struct ColorRound {
    pub function: String,
    /// "GPR" or "FPR"
    pub class: String,
    pub nreal: u32,
    pub first_fe_temp: u32,
    pub first_temp: u32,
    pub nodes: Vec<IgNode>,
}

/// One copy-propagation decision: may the copy `mr vX, vY` be propagated into this use of vX?
/// A copy disappears (its target becomes the source register) only if every use accepts.
#[derive(Clone, Debug, Serialize)]
pub struct CopyPropDecision {
    pub function: String,
    /// the copy (`mr vX, vY` / `fmr`)
    pub copy: String,
    /// the use of vX
    pub use_text: String,
    pub accepted: bool,
    pub reason: String,
}

/// A copy all of whose uses accepted: removed, or kept by the propagation pass (exception
/// register, or a physical source in the non-aggressive pass).
#[derive(Clone, Debug, Serialize)]
pub struct CopyRemoval {
    pub function: String,
    pub copy: String,
    pub removed: bool,
}

/// A peephole rule that changed an instruction (post-RA, physical registers).
#[derive(Clone, Debug, Serialize)]
pub struct PeepholeHit {
    pub function: String,
    pub rule: String,
    pub before: String,
    /// the instruction after the rule (None: removed)
    pub after: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PcodeDump {
    pub stage: String,
    pub function: String,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Trace {
    pub inline: Vec<InlineDecision>,
    /// functions in the order their bodies were expanded (= code generation order)
    pub expand_order: Vec<String>,
    pub rounds: Vec<ColorRound>,
    /// list-scheduler DAGs and picks per basic block (pre-RA and post-RA passes)
    pub sched: Vec<crate::sched::SchedBlock>,
    /// PCode copy propagation: every (copy, use) decision
    pub copyprop: Vec<CopyPropDecision>,
    /// PCode copy propagation: copies whose uses all accepted (removed or kept)
    pub copyprop_removals: Vec<CopyRemoval>,
    /// post-RA peephole rule hits (and dead instructions removed)
    pub peephole: Vec<PeepholeHit>,
    pub pcode: Vec<PcodeDump>,
    /// the front-end optimizer's own dump (IRO passes, expression trees), when requested
    pub iro: String,
    #[serde(skip)]
    pub object: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct TraceOptions {
    pub inline: bool,
    pub coloring: bool,
    /// any of "initial", "presched", "precolor"
    pub pcode: Vec<String>,
    /// enable the compiler's built-in IRO dump (`Trace::iro`): the final form of every function
    pub iro: bool,
    /// with `iro`: dump the flowgraph after every IRO pass, not only the final one (large)
    pub iro_all_stages: bool,
    /// record the list scheduler (DAG + every pick) per basic block (`Trace::sched`)
    pub sched: bool,
    /// with `sched`: only functions whose name contains this
    pub sched_filter: Option<String>,
    /// record PCode copy-propagation decisions (`Trace::copyprop`)
    pub copyprop: bool,
    /// record post-RA peephole rule hits (`Trace::peephole`)
    pub peephole: bool,
}
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
    let variables = crate::oracle_trace::variables(opts, &log);
    let run = crate::wasm::run_oracle(&exe, &args, &comp.work, std::time::Duration::from_secs(300), &variables);
    let _ = std::fs::remove_file(&src);
    let run = run.with_context(|| format!("tracing with {}", exe.display()))?;
    if run.timed_out {
        bail!("tracer: timeout waiting for the compiler");
    }
    let mut trace = crate::oracle_trace::parse(&run.trace, opts);
    if opts.iro {
        trace.iro = std::fs::read(&log).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    }
    let _ = std::fs::remove_file(&log);
    trace.object = std::fs::read(&obj).with_context(|| "tracer: compiler produced no object (compile error?)")?;
    let _ = std::fs::remove_file(&obj);
    Ok(trace)
}

/// One rendered PCode instruction plus its register operands (for dependence classification).
#[derive(Clone, Debug, Default)]
pub(crate) struct PInstr {
    pub(crate) op: i16,
    pub(crate) text: String,
    /// (class, reg, read, write)
    pub(crate) regs: Vec<(u8, i16, bool, bool)>,
}

/// Render one PCode instruction: opcode `op` named `name`, its flags word (0x14) and its `argc` 12-byte operands
/// `raw`, with `object` naming the objects operands refer to.
pub(crate) fn render_pcode(
    op: i16,
    argc: i16,
    flags: u32,
    raw: &[u8],
    name: String,
    mut object: impl FnMut(u32) -> String,
) -> PInstr {
    let mut args = vec![];
    let mut regs = vec![];
    for k in 0..(argc as usize).min(raw.len() / 12) {
        let a = &raw[12 * k..12 * k + 12];
        let (kind, cls) = (a[0], a[1]);
        let eff = u16::from_le_bytes([a[2], a[3]]);
        let reg = i16::from_le_bytes([a[4], a[5]]);
        let imm = i32::from_le_bytes([a[2], a[3], a[4], a[5]]);
        args.push(match kind {
            0 => {
                let pre = match cls {
                    4 => "r",
                    3 => "f",
                    1 => "cr",
                    0 => "spr",
                    _ => "v",
                };
                regs.push((cls, reg, eff & 1 != 0, eff & 2 != 0));
                format!("{pre}{reg}{}", if eff & 3 == 2 { "=" } else { "" })
            }
            2 => format!("#{imm}"),
            3 => {
                let obj = u32::from_le_bytes([a[6], a[7], a[8], a[9]]);
                let n = if obj == 0 { "?".to_string() } else { object(obj) };
                if imm == 0 {
                    n
                } else {
                    format!("{n}+{imm}")
                }
            }
            _ => format!("<{kind}:{imm}>"),
        });
    }
    if matches!(name.as_str(), "bl" | "bctrl" | "blrl") && args.len() > 4 {
        let w = args.iter().filter(|a| a.ends_with('=')).count();
        args.truncate(1);
        args.push(format!("({} operands, {} written regs)", argc, w));
    }
    let mut text = format!("{:<8} {}", name, args.join(", ")).trim_end().to_string();
    // `MWDEC_PCODE_FLAGS`: the alias class the scheduler sees on memory operations (word 0x14 of
    // the PCode): `{ptr}` = through a pointer of unknown origin (ordered against every store and
    // the prologue's register saves), `{const}` = marked read-only; neither = a known object (a
    // stack/global object, or the pointee of a pointer-to-const parameter: never ordered against
    // the register saves)
    if std::env::var_os("MWDEC_PCODE_FLAGS").is_some() && args.iter().any(|a| a.starts_with('#') || a.contains('+')) && (name.starts_with('l') || name.starts_with("st") || name.starts_with("psq")) && !matches!(name.as_str(), "li" | "lis") {
        if flags & 0x20 != 0 {
            text.push_str(" {ptr}");
        }
        if flags & 0x40 != 0 {
            text.push_str(" {const}");
        }
    }
    PInstr { op, text, regs }
}

impl ColorRound {
    /// Nodes that received a callee-saved register, in colouring order.
    pub fn callee_saved(&self) -> Vec<&IgNode> {
        let lo = if self.class == "FPR" { 14 } else { 14 };
        let mut v: Vec<&IgNode> =
            self.nodes.iter().filter(|n| n.reg.map_or(false, |r| r >= lo) && n.coalesced_into.is_none()).collect();
        v.sort_by_key(|n| n.order.unwrap_or(usize::MAX));
        v
    }
}

// ------------------------------------------------------------------ targeted register fixes

impl Trace {
    /// Parsed IRO dump (needs `TraceOptions::iro`).
    pub fn iro_stages(&self) -> Vec<crate::iro::IroStage> {
        crate::iro::parse(&self.iro)
    }

    /// The colouring round of the first function whose name contains `func` for "GPR"/"FPR".
    pub fn round(&self, func: &str, class: &str) -> Option<&ColorRound> {
        self.rounds.iter().find(|r| r.function.contains(func) && r.class == class)
    }
}

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
        for n in by_reg(from) {
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
            break;
        }
    }
    out
}
