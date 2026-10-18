//! The tracer in process: the compiler built with mwcc-tools' oracle hooks (`oracle.wasm`, [`crate::wasm`]) writes
//! records of what it decides, read here into the [`Trace`] the debugger tracer ([`crate::tracer`]) gives.
//!
//! - colouring: `ra.graph` (the class), `ra.node` (each node before simplification), `ra.temporaries` and `ra.value`
//!   (the class's temporaries; each node's object, argument and rematerialization), `ra.order` (the stack select
//!   takes, with each node's spill cost) and `ra.select` (the colour and flags each took), in the function of the
//!   `ra.before` snapshot they follow;
//! - scheduler: `sc.block`, its instructions (`sc.i`), the DAG (`sc.node`) and each pick (`sc.pick`);
//! - inliner: each check (`inl.can`, `inl.decide`, and `inl.cost` for the size estimate it made) in the function
//!   `inl.expand` began;
//! - PCode: snapshots (`st.*`: `st.codegen`, as the global optimizer begins, `st.presched`, as the first scheduling
//!   pass begins, and `st.precolor.*`, as select colours a class), copy propagation (`cp.use`, each decision on a
//!   copy's use, `cp.remove`, each copy all uses took) and the peephole (`ph.hit`, a pattern that changed an
//!   instruction, `ph.dead`, a dead one removed);
//! - the optimizer's dump, into the file `MWCC_ORACLE_IRO_DUMP` names, after the phases `MWCC_ORACLE_IRO_PHASES`
//!   names;
//! - names: `pc.opcode` (an opcode's name, the first time) and `pc.object` (the object an instruction's operand
//!   refers to, after it).
use crate::sched::{SchedBlock, SchedNode, SchedPick};
use crate::tracer::{
    render_pcode, ColorRound, CopyPropDecision, CopyRemoval, IgNode, InlineDecision, PInstr, PcodeDump, PeepholeHit, Trace,
    TraceOptions, VregKind,
};
use std::collections::{BTreeMap, HashMap};

/// The trace variables `opts` needs; `iro`: the file the optimizer's dump goes to.
pub fn variables(opts: &TraceOptions, iro: &std::path::Path) -> Vec<(&'static str, String)> {
    let mut v = vec![("MWCC_ORACLE_NAMES", "1".to_string())];
    if opts.sched {
        v.push(("MWCC_ORACLE_SCHEDULE", "1".into()));
    }
    if !opts.pcode.is_empty() {
        v.push(("MWCC_ORACLE_STAGES", "1".into()));
    }
    if opts.copyprop {
        v.push(("MWCC_ORACLE_COPYPROP", "1".into()));
    }
    if opts.peephole {
        v.push(("MWCC_ORACLE_PEEPHOLE", "1".into()));
    }
    if opts.iro {
        v.push(("MWCC_ORACLE_IRO_DUMP", iro.to_string_lossy().into_owned()));
        let phases = if opts.iro_all_stages { "all" } else { crate::iro::FINAL_STAGE };
        v.push(("MWCC_ORACLE_IRO_PHASES", phases.into()));
    }
    v
}

/// One record: its kind and the rest of its line.
struct Record<'a> {
    kind: &'a str,
    rest: &'a str,
}

impl<'a> Record<'a> {
    /// The value of the field `key` (up to the next space).
    fn get(&self, key: &str) -> Option<&'a str> {
        self.rest.split_whitespace().find_map(|t| t.strip_prefix(key).and_then(|t| t.strip_prefix('=')))
    }

    fn int(&self, key: &str) -> i64 {
        self.get(key).and_then(|v| v.parse().ok()).unwrap_or(0)
    }

    fn hex(&self, key: &str) -> u32 {
        self.get(key).and_then(|v| u32::from_str_radix(v, 16).ok()).unwrap_or(0)
    }

    /// The value of the field `key` to the end of the line.
    fn tail(&self, key: &str) -> &'a str {
        let pattern = format!("{key}=");
        let start = if self.rest.starts_with(&pattern) {
            Some(0)
        } else {
            self.rest.find(&format!(" {pattern}")).map(|i| i + 1)
        };
        start.map_or("", |i| self.rest[i + pattern.len()..].trim_end())
    }
}

/// The records of `text`: its lines that are records (the driver writes a line or two of its own there).
fn records(text: &str) -> Vec<Record<'_>> {
    text.lines()
        .filter_map(|line| {
            let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
            let record = kind.split_once('.').is_some_and(|(a, b)| {
                !a.is_empty() && a.bytes().all(|c| c.is_ascii_lowercase()) && b.bytes().all(|c| c.is_ascii_lowercase() || c == b'.')
            });
            record.then_some(Record { kind, rest })
        })
        .collect()
}

/// A PCode instruction record (`*.i`), rendered with the opcode and object names around it.
struct Instruction {
    pointer: u32,
    rendered: PInstr,
}

/// Read the instruction record `records[at]` and the `pc.object` records after it.
fn instruction(records: &[Record], at: usize, opcodes: &HashMap<i16, (String, u8)>) -> Instruction {
    let r = &records[at];
    let op = r.int("op") as i16;
    let argc = r.int("n") as i16;
    let raw: Vec<u8> = r
        .tail("ops")
        .split_whitespace()
        .flat_map(|g| (0..g.len() / 2).map(move |i| u8::from_str_radix(&g[2 * i..2 * i + 2], 16).unwrap_or(0)))
        .collect();
    let objects: HashMap<u32, &str> = records[at + 1..]
        .iter()
        .take_while(|o| matches!(o.kind, "pc.object" | "pc.opcode"))
        .filter(|o| o.kind == "pc.object")
        .map(|o| (o.hex("p"), o.get("name").unwrap_or("?")))
        .collect();
    let rendered = if !(0..471).contains(&op) || !(0..200).contains(&argc) {
        PInstr { op, name: "??".into(), text: format!("?? op={op} argc={argc}"), regs: vec![] }
    } else {
        let name = opcodes.get(&op).map_or_else(|| format!("op{op}"), |(n, _)| n.clone());
        render_pcode(op, argc, r.hex("flags"), &raw, name, |p| objects.get(&p).map_or("?".into(), |n| n.to_string()))
    };
    Instruction { pointer: r.hex("p"), rendered }
}

/// A colouring round, from its `ra.graph` to its `ra.select`.
#[derive(Default)]
struct Graph {
    class: u8,
    start: u32,
    first_fe_temp: u32,
    first_temp: u32,
    /// vreg -> (pointer, degree at simplification, flags, colour, neighbours)
    nodes: BTreeMap<u16, (u32, u16, u16, i16, Vec<i16>)>,
    /// vreg -> (object, rematerializable, argument, name)
    values: HashMap<u16, (u32, bool, bool, String)>,
    /// the stack: (pointer, spill cost)
    order: Vec<(u32, i32)>,
}

impl Graph {
    fn round(self, function: &str) -> ColorRound {
        let by_pointer: HashMap<u32, u16> = self.nodes.iter().map(|(&v, n)| (n.0, v)).collect();
        let order: Vec<u16> = self.order.iter().filter_map(|(p, _)| by_pointer.get(p).copied()).collect();
        let costs: HashMap<u16, i32> =
            self.order.iter().filter_map(|(p, c)| by_pointer.get(p).map(|&v| (v, *c))).collect();
        let position: HashMap<u16, usize> = order.iter().enumerate().map(|(k, &v)| (v, k)).collect();
        // (blocked: in colouring order, runs of descending vregs; all but the last run were pushed in later
        // simplify passes)
        let mut run_of: HashMap<u16, usize> = HashMap::new();
        let mut run = 0usize;
        for (k, &v) in order.iter().enumerate() {
            if k > 0 && v > order[k - 1] {
                run += 1;
            }
            run_of.insert(v, run);
        }
        let nreal = self.start;
        let resolve = |mut colour: i16| -> Option<u8> {
            let mut guard = 0;
            while colour >= nreal as i16 && guard < 64 {
                colour = self.nodes.get(&(colour as u16)).map_or(-1, |n| n.3);
                guard += 1;
            }
            (0..32).contains(&colour).then_some(colour as u8)
        };
        let nodes = self
            .nodes
            .iter()
            .map(|(&v, (_, degree, flags, colour, neighbours))| {
                let (object, remat, argument, name) =
                    self.values.get(&v).cloned().unwrap_or((0, false, false, String::new()));
                let kind = if object != 0 && argument {
                    VregKind::Param
                } else if u32::from(v) < self.first_fe_temp {
                    VregKind::Named
                } else if u32::from(v) < self.first_temp {
                    VregKind::FeTemp
                } else {
                    VregKind::Temp
                };
                IgNode {
                    vreg: v,
                    name: (object != 0).then_some(name),
                    kind,
                    degree: neighbours.len() as u16,
                    reg: resolve(*colour),
                    coalesced_into: (flags & 4 != 0).then_some(*colour),
                    spilled: flags & 1 != 0,
                    neighbours: neighbours.clone(),
                    order: position.get(&v).copied(),
                    blocked: run_of.get(&v).is_some_and(|&r| r < run),
                    spill_cost: costs.get(&v).copied().unwrap_or(0),
                    remat,
                    simplify_degree: *degree,
                }
            })
            .collect();
        ColorRound {
            function: function.to_string(),
            class: match self.class {
                3 => "FPR".into(),
                4 => "GPR".into(),
                c => format!("class{c}"),
            },
            nreal,
            first_fe_temp: self.first_fe_temp,
            first_temp: self.first_temp,
            nodes,
        }
    }
}

/// A scheduled block, from its `sc.block` to its `sc.after`.
struct Block {
    pre_ra: bool,
    index: u32,
    instructions: Vec<Instruction>,
    /// instruction -> (latency, height, deadline, predecessors, successors and their latencies)
    nodes: HashMap<u32, (u16, u16, u16, u16, Vec<(u32, u16)>)>,
    picks: Vec<(u16, u32)>,
}

impl Block {
    fn finish(self, function: &str, opcodes: &HashMap<i16, (String, u8)>) -> Option<SchedBlock> {
        // (a block the scheduler made no DAG for, as the debugger tracer has it)
        if self.nodes.is_empty() {
            return None;
        }
        let index: HashMap<u32, usize> = self.instructions.iter().enumerate().map(|(i, x)| (x.pointer, i)).collect();
        let mut regs = vec![];
        let mut nodes = vec![];
        for x in self.instructions {
            let (latency, height, deadline, preds, succs) = self.nodes.get(&x.pointer).cloned().unwrap_or_default();
            regs.push(x.rendered.regs.clone());
            nodes.push(SchedNode {
                opcode_rank: opcodes.get(&x.rendered.op).map_or(0, |o| o.1),
                text: x.rendered.text,
                latency,
                height,
                deadline,
                preds,
                succs: vec![],
                raw_succs: succs.iter().filter_map(|(p, l)| index.get(p).map(|&i| (i, *l))).collect(),
            });
        }
        Some(SchedBlock {
            function: function.to_string(),
            pre_ra: self.pre_ra,
            block: self.index,
            nodes: crate::sched::classify_edges(nodes, &regs),
            picks: self
                .picks
                .iter()
                .filter_map(|&(cycle, p)| index.get(&p).map(|&node| SchedPick { cycle, node }))
                .collect(),
        })
    }
}

/// Why `CInline_InlineFunctionCheck` decided as it did, by its own tests in order: an inline candidate; always
/// inline; bottom-up (`auto`): a body, the size estimate against the limit; otherwise, in a nested pass without an
/// inline depth, a body and the statement limits of the pass.
fn reason(can: &Record, candidate: bool, pass: i64, cost: Option<i32>, max: i32) -> &'static str {
    let body = can.int("body") != 0;
    let statements = can.int("nstmts");
    if !candidate {
        "not_inline_candidate"
    } else if can.int("always") != 0 {
        "always_inline"
    } else if can.int("auto") != 0 {
        if !body {
            "no_body_yet"
        } else if cost.is_some_and(|c| c > max) {
            "too_big"
        } else {
            "ok"
        }
    } else if pass > 0 && can.int("limit") == 0 {
        if !body {
            "no_body_yet"
        } else if statements > 10 {
            "stmt_limit_10"
        } else if pass > 1 && statements > 7 {
            "stmt_limit_7"
        } else if pass > 2 && statements > 3 {
            "stmt_limit_3"
        } else {
            "ok"
        }
    } else {
        "ok"
    }
}

/// The trace of the records `text`, for `opts`.
pub fn parse(text: &str, opts: &TraceOptions) -> Trace {
    let records = records(text);
    let opcodes: HashMap<i16, (String, u8)> = records
        .iter()
        .filter(|r| r.kind == "pc.opcode")
        .map(|r| (r.int("op") as i16, (r.get("name").unwrap_or("?").to_lowercase(), r.int("rank") as u8)))
        .collect();
    let mut t = Trace::default();
    let mut function = String::new();
    let mut allocating = String::new();
    let mut graph: Option<Graph> = None;
    let mut block: Option<Block> = None;
    let mut can: Option<usize> = None;
    let mut cost: Option<(i32, i32)> = None;
    let mut dump: Option<PcodeDump> = None;
    let expanding = opts.inline || opts.coloring || opts.sched || opts.copyprop || opts.peephole || !opts.pcode.is_empty();
    let scheduled = |function: &str| opts.sched_filter.as_ref().is_none_or(|f| function.contains(f.as_str()));
    for (at, r) in records.iter().enumerate() {
        match r.kind {
            "inl.expand" => {
                function = r.get("function").unwrap_or("?").to_string();
                if expanding {
                    t.expand_order.push(function.clone());
                }
                cost = None;
            }
            "inl.cost" if r.int("nest") == 0 => cost = Some((r.int("cost") as i32, r.int("depth") as i32)),
            "inl.can" => can = Some(at),
            "inl.decide" if opts.inline => {
                let Some(c) = can.take().map(|i| &records[i]) else { continue };
                let candidate = r.int("candidate") != 0;
                let pass = r.int("pass");
                let max = r.int("max") as i32;
                // (the size estimate is the check's own only on the bottom-up path)
                let estimated = candidate && c.int("always") == 0 && c.int("auto") != 0 && c.int("body") != 0;
                let (cost, level) = match cost.take() {
                    Some((c, l)) if estimated => (Some(c), Some(l)),
                    _ => (None, None),
                };
                t.inline.push(InlineDecision {
                    caller: r.get("caller").unwrap_or("?").to_string(),
                    callee: r.get("callee").unwrap_or("?").to_string(),
                    pass: pass as i16,
                    level,
                    cost,
                    max,
                    inlined: c.int("result") != 0,
                    reason: reason(c, candidate, pass, cost, max).to_string(),
                });
            }
            "ra.before" => allocating = r.get("function").unwrap_or("?").to_string(),
            "ra.graph" if opts.coloring => {
                graph = Some(Graph { class: r.int("cls") as u8, start: r.int("start") as u32, ..Default::default() })
            }
            "ra.node" => {
                if let Some(g) = graph.as_mut() {
                    let neighbours = r.get("n").unwrap_or("").split(',').filter_map(|n| n.parse().ok()).collect();
                    let node = (r.hex("p"), r.int("deg") as u16, r.hex("flags") as u16, r.int("color") as i16, neighbours);
                    g.nodes.insert(r.int("v") as u16, node);
                }
            }
            "ra.temporaries" => {
                if let Some(g) = graph.as_mut() {
                    g.first_fe_temp = r.int("fe") as u32;
                    g.first_temp = r.int("first") as u32;
                }
            }
            "ra.value" => {
                if let Some(g) = graph.as_mut() {
                    let value = (r.hex("object"), r.int("remat") != 0, r.int("argument") != 0, r.tail("name").to_string());
                    g.values.insert(r.int("v") as u16, value);
                }
            }
            "ra.order" => {
                if let Some(g) = graph.as_mut() {
                    g.order = r
                        .get("v")
                        .unwrap_or("")
                        .split(',')
                        .filter_map(|e| e.split_once(':'))
                        .map(|(p, c)| (u32::from_str_radix(p, 16).unwrap_or(0), c.parse().unwrap_or(0)))
                        .collect();
                }
            }
            "ra.select" => {
                if let Some(mut g) = graph.take() {
                    let by_pointer: HashMap<u32, u16> = g.nodes.iter().map(|(&v, n)| (n.0, v)).collect();
                    for entry in r.get("v").unwrap_or("").split(',').filter(|e| !e.is_empty()) {
                        let mut parts = entry.split(':');
                        let (Some(p), Some(colour), Some(flags)) = (parts.next(), parts.next(), parts.next()) else {
                            continue;
                        };
                        let Some(v) = u32::from_str_radix(p, 16).ok().and_then(|p| by_pointer.get(&p)) else { continue };
                        if let Some(n) = g.nodes.get_mut(v) {
                            n.3 = colour.parse().unwrap_or(-1);
                            n.2 = u16::from_str_radix(flags, 16).unwrap_or(0);
                        }
                    }
                    t.rounds.push(g.round(&allocating));
                }
            }
            "sc.block" if opts.sched => {
                block = scheduled(&function).then(|| Block {
                    pre_ra: r.int("virtual") != 0,
                    index: r.int("index") as u32,
                    instructions: vec![],
                    nodes: HashMap::new(),
                    picks: vec![],
                })
            }
            "sc.i" => {
                if let Some(b) = block.as_mut() {
                    b.instructions.push(instruction(&records, at, &opcodes));
                }
            }
            "sc.node" => {
                if let Some(b) = block.as_mut() {
                    let succs = r
                        .get("succ")
                        .unwrap_or("")
                        .split(',')
                        .filter_map(|e| e.split_once(':'))
                        .map(|(p, l)| (u32::from_str_radix(p, 16).unwrap_or(0), l.parse().unwrap_or(0)))
                        .collect();
                    let node = (r.int("latency") as u16, r.int("height") as u16, r.int("latest") as u16, r.int("preds") as u16, succs);
                    b.nodes.insert(r.hex("i"), node);
                }
            }
            "sc.pick" => {
                if let Some(b) = block.as_mut() {
                    let pick = r.hex("pick");
                    if pick != 0 {
                        b.picks.push((r.int("cycle") as u16, pick));
                    }
                }
            }
            "sc.after" => {
                if let Some(b) = block.take().and_then(|b| b.finish(&function, &opcodes)) {
                    t.sched.push(b);
                }
            }
            kind if kind.starts_with("st.") && !matches!(kind, "st.block" | "st.i" | "st.end") => {
                let stage = match kind {
                    "st.codegen" => "initial",
                    "st.presched" => "presched",
                    "st.precolor.gpr" => "precolor-gpr",
                    "st.precolor.fpr" => "precolor-fpr",
                    _ => "",
                };
                let wanted = opts.pcode.iter().any(|s| s == stage || s == "precolor" && stage.starts_with("precolor-"));
                // (the debugger names the function select colours by its object, the others by the expansion)
                let name = if kind.starts_with("st.precolor") { r.get("function").unwrap_or("?") } else { &function };
                dump = wanted.then(|| PcodeDump { stage: stage.into(), function: name.to_string(), lines: vec![] });
            }
            "st.block" => {
                if let Some(d) = dump.as_mut() {
                    d.lines.push(format!("B{}: weight {}", r.int("index"), r.int("weight")));
                }
            }
            "st.i" => {
                if let Some(d) = dump.as_mut() {
                    d.lines.push(format!("   {}", instruction(&records, at, &opcodes).rendered.text));
                }
            }
            "st.end" => t.pcode.extend(dump.take()),
            "cp.use" if opts.copyprop => {
                let copy = after(&records, at, "cp.copy");
                let into = copy.and_then(|c| after(&records, c, "cp.into"));
                let reason = usize::try_from(r.int("reason")).ok().and_then(|i| COPY_REASONS.get(i));
                if let (Some(copy), Some(into), Some(reason)) = (copy, into, reason) {
                    t.copyprop.push(CopyPropDecision {
                        function: function.clone(),
                        copy: instruction(&records, copy, &opcodes).rendered.text,
                        use_text: instruction(&records, into, &opcodes).rendered.text,
                        accepted: r.int("accepted") != 0,
                        reason: reason.to_string(),
                    });
                }
            }
            "cp.remove" if opts.copyprop => {
                if let Some(copy) = after(&records, at, "cp.copy") {
                    t.copyprop_removals.push(CopyRemoval {
                        function: function.clone(),
                        copy: instruction(&records, copy, &opcodes).rendered.text,
                        removed: r.int("removed") != 0,
                    });
                }
            }
            "ph.hit" | "ph.dead" if opts.peephole => {
                let Some(before) = after(&records, at, "ph.before") else { continue };
                let rule = match r.kind {
                    "ph.dead" => "dead_instruction".to_string(),
                    _ => {
                        let pattern = r.get("rule").unwrap_or("?");
                        PATTERNS.iter().find(|p| p.0 == pattern).map_or_else(|| pattern.to_string(), |p| p.1.to_string())
                    }
                };
                t.peephole.push(PeepholeHit {
                    function: function.clone(),
                    rule,
                    before: instruction(&records, before, &opcodes).rendered.text,
                    after: after(&records, before, "ph.after").map(|a| instruction(&records, a, &opcodes).rendered.text),
                });
            }
            _ => {}
        }
    }
    t
}

/// The record of kind `kind` that follows `records[at]`, past the names (`pc.*`) after it.
fn after(records: &[Record], at: usize, kind: &str) -> Option<usize> {
    let next = (at + 1..records.len()).find(|&i| !records[i].kind.starts_with("pc."))?;
    (records[next].kind == kind).then_some(next)
}

/// Why a copy does or does not propagate into a use (`cp.use reason=`), as the debugger tracer says it.
const COPY_REASONS: [&str; 6] = [
    "use is a move (copies never propagate into moves)",
    "use reads and writes the copied register",
    "source redefined between copy and use",
    "copy does not reach the use on every path",
    "source redefined in the use's block before the use",
    "ok",
];

/// The peephole's patterns (gc2's names, `ph.hit rule=`) and the names the debugger tracer gives them
/// ([`crate::tracer::PEEPHOLE_RULES`]). (gc2's sources have them in the executable's order, reversed.)
const PATTERNS: &[(&str, &str)] = &[
    ("RLWINM_RLWIMI_STH", "fold_rlwimi_rlwinm_to_sthbrx"),
    ("RLWINM_RLWIMI_STW", "fold_rlwimi_store_to_stwbrx"),
    ("SRW_SUBFIC_RLW_OR", "fold_to_rlwnm"),
    ("MR_ADDI", "rewrite_as_addi"),
    ("SRAWI_SRAWI", "combine_srawi"),
    ("ADDI_ADDI", "combine_addi"),
    ("MULLI_MULLI", "combine_mulli"),
    ("RLWINM_RLWINM", "fold_rlwinm_or_mr_reaching_def"),
    ("L_S", "eliminate_redundant_store"),
    ("L_FMR", "rewrite_reaching_def_reg"),
    ("L_MR", "retarget_reaching_def_register"),
    ("VSPLTIS_VMR", "merge_reaching_def_instruction"),
    ("LI_MR", "replace_with_reaching_li"),
    ("NOT_AND", "fold_not_into_andc"),
    ("LBZ_EXTSB_CMPI_BC", "remove_redundant_extsb"),
    ("RLWINM_CMPLI_BC", "bypass_cmpli_zero"),
    ("LI_CMP_BC", "fold_constant_compare_branch"),
    ("L_S_ADDI", "eliminate_matching_addi"),
    ("ADDI_LU_SU", "fold_reaching_addi"),
    ("ADDI_L_S", "fold_addi_or_mr_reaching_def"),
    ("LI_L_S", "fold_li_operand"),
    ("LHA_EXTSH", "fold_reaching_lha_or_extsb"),
    ("LHZ_RLWINM", "fold_lhz_lhzx_mask"),
    ("LBZ_RLWINM", "fold_lbz_lbzx_mask"),
    ("EXTSH_RLWINM", "bypass_extsh_for_rotated_mask"),
    ("EXTSB_RLWINM", "bypass_extsb_for_low_byte_rotated_mask"),
    ("MR_CMPI", "make_record_form"),
    ("VMR_VMRP", "peephole_rule_506ef0"),
    ("VMR_VMR", "unlink_vmr_matching_source"),
    ("FMR_FMR", "unlink_unmatched_fmr"),
    ("MR_MR", "unlink_mr_with_matching_def"),
    ("VMR_Vx_Vx", "unlink_same_reg_move"),
    ("FMR_Fx_Fx", "unlink_same_reg_instruction"),
    ("MR_Rx_Rx", "unlink_equal_reg_instruction"),
];
