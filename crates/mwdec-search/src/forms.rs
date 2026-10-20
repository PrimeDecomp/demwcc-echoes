//! Source forms from hand fixes of Echoes near misses (functions at 95-99.9% that `mwdec match`
//! could not finish, 2026-10-19/20; none of them a dataset row): a ternary argument as an if/else
//! around the whole call, tail duplication of an else, explicit `== true` tests, empty cases,
//! `break`/`return` at the end of a void function, an early return inverted into if/else, an
//! increment folded into its test, function-local literal constants, vector component forms and
//! a float view of a vector local. Same contract as [`crate::ops`]: best-effort semantics
//! preserving rewrites inside the function body; the strict comparator is the judge.
use crate::cst::{normalize, strip_parens, Cst, Edit};
use crate::func::{self, infer_type, is_loop, is_scalar_type};
use crate::ops::{and_operand, braced, cond_expr, ends_with_return, has_kind, in_cond_context, indent_of, inner_block, mentions, names_declared, negate, ptext, ptext_bin, pure, remove_stmt, replace_stmt_many, single, stmts_of, ternary_branch, uses_of, M};

/// Text of statement `s` with node `n` (inside it) replaced by `with`.
fn text_with(c: &Cst, s: usize, n: usize, with: &str) -> String {
    format!("{}{with}{}", &c.src[c.nodes[s].start..c.nodes[n].start], &c.src[c.nodes[n].end..c.nodes[s].end])
}

/// `n` without enclosing parentheses (the node itself otherwise).
fn unparen(c: &Cst, mut n: usize) -> usize {
    while c.kind(n) == "parenthesized_expression" {
        match c.named(n).first() {
            Some(&i) => n = i,
            None => break,
        }
    }
    n
}

/// Condition text for an `if (...)` or a ternary condition (no outer parentheses).
fn cond_text(c: &Cst, e: usize) -> String {
    strip_parens(c.text(e)).to_string()
}

/// Ternary condition operand: parenthesized when it is an assignment, comma or ternary itself.
fn ternary_cond(c: &Cst, e: usize) -> String {
    let e = unparen(c, e);
    match c.kind(e) {
        "conditional_expression" | "assignment_expression" | "comma_expression" => format!("({})", c.text(e)),
        _ => c.text(e).to_string(),
    }
}

// ------------------------------------------------------------------ ternary argument <-> if/else

/// The statement-level call whose direct argument is ternary `n` (through parentheses):
/// (statement, call, argument index).
fn call_arg_site(c: &Cst, n: usize) -> Option<(usize, usize, usize)> {
    let mut a = n;
    let mut p = c.parent(a)?;
    while c.kind(p) == "parenthesized_expression" {
        a = p;
        p = c.parent(a)?;
    }
    if c.kind(p) != "argument_list" {
        return None;
    }
    let call = c.parent(p).filter(|&x| c.kind(x) == "call_expression")?;
    let s = c.parent(call).filter(|&x| c.kind(x) == "expression_statement")?;
    let k = c.named(p).iter().position(|&x| x == a)?;
    Some((s, call, k))
}

/// The parts of a call other than argument `k` (callee and the other arguments) are pure.
fn call_rest_pure(c: &Cst, call: usize, k: usize) -> bool {
    let Some(f) = c.child(call, "function") else { return false };
    let args = c.child(call, "arguments").map(|a| c.named(a)).unwrap_or_default();
    pure(c, f) && args.iter().enumerate().all(|(i, &a)| i == k || pure(c, a))
}

/// `f(x, c ? a : b);` <-> `if (c) { f(x, a); } else { f(x, b); }` (the call duplicated into
/// both arms: each arm passes its own value, the select disappears). The condition is evaluated
/// before the other arguments in the if-form, so it or the rest of the call must be pure.
pub fn op_call_ternary(m: &mut M) -> Option<Vec<Edit>> {
    let c = m.cst;
    let mut fwd = Vec::new();
    for n in m.of_kind(&["conditional_expression"]) {
        let Some((s, call, k)) = call_arg_site(c, n) else { continue };
        let Some(cond) = c.child(n, "condition") else { continue };
        if pure(c, cond) || call_rest_pure(c, call, k) {
            fwd.push((s, n, cond));
        }
    }
    let mut back = Vec::new();
    for s in m.of_kind(&["if_statement"]) {
        let (Some(ce), Some(cons), Some(alt)) = (cond_expr(c, s), c.child(s, "consequence"), c.child(s, "alternative")) else { continue };
        let Some(a) = single(c, cons) else { continue };
        let Some(b) = c.named(alt).first().and_then(|&x| single(c, x)) else { continue };
        let call_of = |t: usize| c.named(t).first().copied().filter(|&e| c.kind(t) == "expression_statement" && c.kind(e) == "call_expression");
        let (Some(ca), Some(cb)) = (call_of(a), call_of(b)) else { continue };
        let (Some(fa), Some(fb)) = (c.child(ca, "function"), c.child(cb, "function")) else { continue };
        let (Some(la), Some(lb)) = (c.child(ca, "arguments"), c.child(cb, "arguments")) else { continue };
        let (aa, ab) = (c.named(la), c.named(lb));
        if normalize(c.text(fa)) != normalize(c.text(fb)) || aa.len() != ab.len() {
            continue;
        }
        let diff: Vec<usize> = (0..aa.len()).filter(|&i| normalize(c.text(aa[i])) != normalize(c.text(ab[i]))).collect();
        let [k] = diff.as_slice() else { continue };
        if pure(c, ce) || call_rest_pure(c, ca, *k) {
            back.push((s, a, aa[*k], ab[*k], ce));
        }
    }
    if !fwd.is_empty() && (back.is_empty() || m.rng.chance(0.5)) {
        let (s, n, cond) = m.pick(&fwd)?;
        let (a, b) = (c.child(n, "consequence")?, c.child(n, "alternative")?);
        let ind = indent_of(c, s);
        let (sa, sb) = (text_with(c, s, n, &ternary_branch(c, a)), text_with(c, s, n, &ternary_branch(c, b)));
        return Some(vec![Edit::replace(c, s, format!("if ({}) {{\n{ind}    {sa}\n{ind}}} else {{\n{ind}    {sb}\n{ind}}}", cond_text(c, cond)))]);
    }
    let (s, a, xa, xb, ce) = m.pick(&back)?;
    let t = format!("{} ? {} : {}", ternary_cond(c, ce), ternary_branch(c, xa), ternary_branch(c, xb));
    Some(vec![Edit::replace(c, s, text_with(c, a, xa, &t))])
}

// ------------------------------------------------------------------ tail duplication

/// A statement that may be duplicated: no labels, case labels or function-local statics.
fn duplicable(c: &Cst, n: usize) -> bool {
    !has_kind(c, n, &["labeled_statement", "case_statement"]) && !c.descendants(n).into_iter().any(|d| c.kind(d) == "declaration" && c.text(d).trim_start().starts_with("static"))
}

/// The statement of an `else` (the alternative clause's body).
fn else_body(c: &Cst, s: usize) -> Option<usize> {
    c.child(s, "alternative").and_then(|a| c.named(a).first().copied())
}

/// Same statements (blocks compared by their contents).
fn same_stmt(c: &Cst, a: usize, b: usize) -> bool {
    normalize(&inner_block(c, a)) == normalize(&inner_block(c, b))
}

/// Tail duplication: `if (a && b) X else Y` <-> `if (a) { if (b) X else Y } else Y` (and
/// `if (a || b) X else Y` <-> `if (a) X else { if (b) X else Y }`): one test per branch, the
/// shared arm emitted twice. Also `if (c) S else S` -> `S` for a pure `c` (an `else if` whose
/// arms are the same code).
pub fn op_tail_dup(m: &mut M) -> Option<Vec<Edit>> {
    let c = m.cst;
    // (if, kind): 0 split &&, 1 split ||, 2 join &&, 3 join ||, 4 drop a test with equal arms
    let mut cands: Vec<(usize, u8)> = Vec::new();
    for s in m.of_kind(&["if_statement"]) {
        let (Some(ce), Some(cons), Some(y)) = (cond_expr(c, s), c.child(s, "consequence"), else_body(c, s)) else { continue };
        let ce0 = unparen(c, ce);
        if c.kind(ce0) == "binary_expression" {
            match c.op(ce0) {
                Some("&&") if duplicable(c, y) => cands.push((s, 0)),
                Some("||") if duplicable(c, cons) => cands.push((s, 1)),
                _ => {}
            }
        }
        if let Some(inner) = single(c, cons).filter(|&i| c.kind(i) == "if_statement" && cond_expr(c, i).is_some()) {
            if else_body(c, inner).is_some_and(|y1| same_stmt(c, y1, y)) {
                cands.push((s, 2));
            }
        }
        if let Some(inner) = single(c, y).filter(|&i| c.kind(i) == "if_statement" && cond_expr(c, i).is_some() && else_body(c, i).is_some()) {
            if c.child(inner, "consequence").is_some_and(|x1| same_stmt(c, x1, cons)) {
                cands.push((s, 3));
            }
        }
        if pure(c, ce) && same_stmt(c, cons, y) {
            cands.push((s, 4));
        }
    }
    let (s, kind) = m.pick(&cands)?;
    let ce = unparen(c, cond_expr(c, s)?);
    let cons = c.child(s, "consequence")?;
    let y = else_body(c, s)?;
    let ind = indent_of(c, s);
    let t = match kind {
        0 | 1 => {
            let (a, b) = (c.child(ce, "left")?, c.child(ce, "right")?);
            let (a, b) = (cond_text(c, a), cond_text(c, b));
            let (x, y) = (braced(c, cons), braced(c, y));
            if kind == 0 {
                format!("if ({a}) {{\n{ind}    if ({b}) {x} else {y}\n{ind}}} else {y}")
            } else {
                format!("if ({a}) {x} else {{\n{ind}    if ({b}) {x} else {y}\n{ind}}}")
            }
        }
        2 => {
            let inner = single(c, cons)?;
            let b = cond_expr(c, inner)?;
            format!("if ({} && {}) {} else {}", and_operand(c, ce), and_operand(c, b), braced(c, c.child(inner, "consequence")?), braced(c, y))
        }
        3 => {
            let inner = single(c, y)?;
            let b = cond_expr(c, inner)?;
            let or = |n: usize| match c.kind(n) {
                "conditional_expression" | "assignment_expression" | "comma_expression" => format!("({})", c.text(n)),
                _ => c.text(n).to_string(),
            };
            format!("if ({} || {}) {} else {}", or(ce), or(unparen(c, b)), braced(c, cons), braced(c, else_body(c, inner)?))
        }
        _ => {
            let in_block = c.parent(s).is_some_and(|p| c.kind(p) == "compound_statement");
            let has_decl = c.kind(cons) == "compound_statement" && stmts_of(c, cons).iter().any(|&x| c.kind(x) == "declaration");
            if in_block && !has_decl {
                inner_block(c, cons)
            } else {
                braced(c, cons)
            }
        }
    };
    Some(vec![Edit::replace(c, s, t)])
}

// ------------------------------------------------------------------ explicit bool comparison

/// Expression of type `bool`: a bool local/parameter, a comparison or `!`, or (with the context's
/// types) a member or method declared `bool` everywhere it is declared.
fn is_bool_expr(m: &M, n: usize) -> bool {
    let c = m.cst;
    let n = unparen(c, n);
    if infer_type(c, m.info, n).is_some_and(|t| t == "bool") && c.kind(n) == "identifier" {
        return true;
    }
    let Some(ctx) = m.ctx else { return false };
    let member = match c.kind(n) {
        "field_expression" => c.child(n, "field"),
        "call_expression" => c.child(n, "function").and_then(|f| match c.kind(f) {
            "field_expression" => c.child(f, "field"),
            "identifier" => Some(f),
            "qualified_identifier" => c.child(f, "name"),
            _ => None,
        }),
        "identifier" if !m.info.vars.contains_key(c.text(n)) => Some(n),
        _ => None,
    };
    member.is_some_and(|f| ctx.bool_members.contains(c.text(f)))
}

/// `if (b)` <-> `if (b == true)`, `!b` <-> `b == false` / `b != true` for bool operands (the
/// explicit compare emits `cmplwi r, 1` instead of a test against zero).
pub fn op_bool_compare(m: &mut M) -> Option<Vec<Edit>> {
    let c = m.cst;
    let mut cands: Vec<(usize, String)> = Vec::new();
    for n in m.nodes.clone() {
        if !c.nodes[n].named {
            continue;
        }
        match c.kind(n) {
            "binary_expression" if matches!(c.op(n), Some("==") | Some("!=")) => {
                let (Some(l), Some(r)) = (c.child(n, "left"), c.child(n, "right")) else { continue };
                let (e, lit) = match (c.kind(r), c.kind(l)) {
                    ("true" | "false", _) => (l, c.text(r)),
                    (_, "true" | "false") => (r, c.text(l)),
                    _ => continue,
                };
                let positive = (lit == "true") == (c.op(n) == Some("=="));
                let t = if positive { ptext(c, e) } else { format!("!{}", ptext(c, e)) };
                // (operands of `!` / `.` keep the grouping)
                let t = if c.parent(n).is_some_and(|p| !matches!(c.kind(p), "condition_clause" | "parenthesized_expression" | "argument_list" | "init_declarator" | "return_statement" | "expression_statement")) && !positive {
                    format!("({t})")
                } else {
                    t
                };
                cands.push((n, t));
            }
            "unary_expression" if c.op(n) == Some("!") => {
                let Some(a) = c.child(n, "argument") else { continue };
                if !is_bool_expr(m, a) {
                    continue;
                }
                let alt = if m.rng.chance(0.5) { "== false" } else { "!= true" };
                let t = format!("{} {alt}", ptext_bin(c, unparen(c, a)));
                let wrap = c.parent(n).is_some_and(|p| !matches!(c.kind(p), "condition_clause" | "parenthesized_expression"));
                cands.push((n, if wrap { format!("({t})") } else { t }));
            }
            _ => {
                if !in_cond_context(c, n) || c.parent(n).is_some_and(|p| c.kind(p) == "unary_expression") || !is_bool_expr(m, n) {
                    continue;
                }
                let t = format!("{} == true", ptext_bin(c, unparen(c, n)));
                let wrap = c.parent(n).is_some_and(|p| matches!(c.kind(p), "binary_expression" | "conditional_expression"));
                cands.push((n, if wrap { format!("({t})") } else { t }));
            }
        }
    }
    let (n, t) = m.pick_one(&cands)?;
    Some(vec![Edit::replace(c, n, t)])
}

// ------------------------------------------------------------------ switch forms

/// `break` statements that belong to `sw` (not to a loop or switch inside it).
fn owned_by(c: &Cst, sw: usize, kind: &str) -> Vec<usize> {
    c.descendants(sw)
        .into_iter()
        .filter(|&d| c.kind(d) == kind)
        .filter(|&d| c.ancestors(d).into_iter().find(|&a| c.kind(a) == "switch_statement" || is_loop(c.kind(a))) == Some(sw))
        .collect()
}

fn is_void_function(m: &M) -> bool {
    let t = m.info.ret_ty.trim();
    t == "void" || t.is_empty()
}

/// In a void function ending with a switch: `break;` <-> `return;` inside its cases (the
/// return jumps straight to the epilogue; one site, or every one of the switch).
pub fn op_case_return(m: &mut M) -> Option<Vec<Edit>> {
    let c = m.cst;
    if !is_void_function(m) {
        return None;
    }
    let sw = *stmts_of(c, m.info.body).last()?;
    if c.kind(sw) != "switch_statement" {
        return None;
    }
    let breaks: Vec<usize> = owned_by(c, sw, "break_statement").into_iter().filter(|b| m.nodes.contains(b)).collect();
    let rets: Vec<usize> = owned_by(c, sw, "return_statement").into_iter().filter(|&r| c.named(r).is_empty() && m.nodes.contains(&r)).collect();
    let (sites, to) = if !breaks.is_empty() && (rets.is_empty() || m.rng.chance(0.5)) { (breaks, "return;") } else { (rets, "break;") };
    let s = m.pick(&sites)?;
    if m.rng.chance(0.5) {
        return Some(sites.iter().map(|&x| Edit::replace(c, x, to)).collect());
    }
    Some(vec![Edit::replace(c, s, to)])
}

/// Statements of a case after its label.
fn case_stmts(c: &Cst, case: usize) -> Vec<usize> {
    let v = c.child(case, "value");
    c.named(case).into_iter().filter(|&s| Some(s) != v && func::is_stmt(c.kind(s))).collect()
}

/// Control leaves the statement list (it doesn't fall into the next case).
fn ends_in_jump(c: &Cst, s: usize) -> bool {
    match c.kind(s) {
        "return_statement" | "break_statement" | "continue_statement" | "goto_statement" => true,
        "compound_statement" => stmts_of(c, s).last().is_some_and(|&l| ends_in_jump(c, l)),
        _ => false,
    }
}

/// In a switch over an enum without `default`, an empty `case K: break;` for an enumerator not
/// present yet (it changes the compare tree / jump table, not what runs), or such an empty case
/// removed. The enum is the context's enum holding every present label.
pub fn op_case_insert(m: &mut M) -> Option<Vec<Edit>> {
    let c = m.cst;
    let ctx = m.ctx?;
    let mut cands: Vec<Edit> = Vec::new();
    for sw in m.of_kind(&["switch_statement"]) {
        let Some(body) = c.child(sw, "body") else { continue };
        let cases: Vec<usize> = c.named(body).into_iter().filter(|&x| c.kind(x) != "comment").collect();
        if cases.is_empty() || cases.iter().any(|&x| c.kind(x) != "case_statement" || c.child(x, "value").is_none()) {
            continue;
        }
        // labels as (enumerator, qualifier prefix)
        let labels: Option<Vec<(String, String)>> = cases
            .iter()
            .map(|&x| {
                let v = unparen(c, c.child(x, "value")?);
                let t = c.text(v);
                match c.kind(v) {
                    "identifier" => Some((t.to_string(), String::new())),
                    "qualified_identifier" => t.rfind("::").map(|i| (t[i + 2..].to_string(), t[..i + 2].to_string())),
                    _ => None,
                }
            })
            .collect();
        let Some(labels) = labels else { continue };
        let Some((_, values)) = ctx.enums.iter().find(|(_, vs)| labels.iter().all(|(l, _)| vs.iter().any(|(v, _)| v == l))) else { continue };
        let present: Vec<i64> = labels.iter().filter_map(|(l, _)| values.iter().find(|(v, _)| v == l).map(|x| x.1)).collect();
        let prefix = labels[0].1.clone();
        let ind = indent_of(c, cases[0]);
        // positions a new case may go: after the last case, or before a case nothing falls into
        let mut spots = vec![(c.nodes[*cases.last().unwrap()].end, true)];
        for (k, &x) in cases.iter().enumerate() {
            if k == 0 || case_stmts(c, cases[k - 1]).last().is_some_and(|&l| ends_in_jump(c, l)) {
                spots.push((c.nodes[x].start, false));
            }
        }
        let mut done: Vec<i64> = present.clone();
        for (name, val) in values {
            if done.contains(val) {
                continue;
            }
            done.push(*val);
            let (at, after) = spots[m.rng.below(spots.len())];
            let t = if after { format!("\n{ind}case {prefix}{name}:\n{ind}    break;") } else { format!("case {prefix}{name}:\n{ind}    break;\n{ind}") };
            cands.push(Edit::insert(at, t));
        }
        for (k, &x) in cases.iter().enumerate() {
            let b = case_stmts(c, x);
            let prev_ok = k == 0 || case_stmts(c, cases[k - 1]).last().is_some_and(|&l| ends_in_jump(c, l));
            if b.len() == 1 && c.kind(b[0]) == "break_statement" && prev_ok && cases.len() > 1 {
                cands.push(remove_stmt(c, x));
            }
        }
    }
    let e = m.pick_one(&cands)?;
    Some(vec![e])
}

// ------------------------------------------------------------------ early return <-> if/else

/// `if (A) { X; return; } R` (to the end of a void function) <-> `if (!A) { R } else { X }`, as
/// one move (early_return + negate_if, minus the return): which arm falls through decides the
/// block order. Also anywhere the code after the `if` ends with a return (the returns are kept).
pub fn op_invert_early(m: &mut M) -> Option<Vec<Edit>> {
    let c = m.cst;
    let void = is_void_function(m);
    let mut fwd = Vec::new();
    let mut back = Vec::new();
    for (blk, l) in m.lists(1) {
        let top = blk == m.info.body;
        for (i, &s) in l.iter().enumerate() {
            if c.kind(s) != "if_statement" || !m.nodes.contains(&s) {
                continue;
            }
            let (Some(_), Some(cons)) = (cond_expr(c, s), c.child(s, "consequence")) else { continue };
            match else_body(c, s) {
                None => {
                    let rest = &l[i + 1..];
                    let Some(&last) = rest.last() else { continue };
                    if !ends_with_return(c, cons) || rest.iter().any(|&t| has_kind(c, t, &["labeled_statement"]) || c.kind(t) == "labeled_statement") {
                        continue;
                    }
                    if (top && void) || c.kind(last) == "return_statement" {
                        fwd.push((s, last, top && void));
                    }
                }
                Some(x) => {
                    if (top && void && i + 1 == l.len()) || ends_with_return(c, x) {
                        back.push(s);
                    }
                }
            }
        }
    }
    if !fwd.is_empty() && (back.is_empty() || m.rng.chance(0.5)) {
        let (s, last, drop_ret) = m.pick(&fwd)?;
        let ce = cond_expr(c, s)?;
        let cons = c.child(s, "consequence")?;
        let mut xs = if c.kind(cons) == "compound_statement" { stmts_of(c, cons) } else { vec![cons] };
        if drop_ret && xs.last().is_some_and(|&r| c.kind(r) == "return_statement" && c.named(r).is_empty()) {
            xs.pop();
        }
        let ind = indent_of(c, s);
        let next = c.src[c.nodes[s].end..].find(|ch: char| !ch.is_whitespace()).map(|k| c.nodes[s].end + k)?;
        let r = &c.src[next..c.nodes[last].end];
        let head = format!("if ({}) {{\n{ind}    {r}\n{ind}}}", negate(c, ce));
        let t = match (xs.first(), xs.last()) {
            (Some(&a), Some(&b)) => format!("{head} else {{\n{ind}    {}\n{ind}}}", &c.src[c.nodes[a].start..c.nodes[b].end]),
            _ => head,
        };
        return Some(vec![Edit { start: c.nodes[s].start, end: c.nodes[last].end, text: t }]);
    }
    let s = m.pick(&back)?;
    let ce = cond_expr(c, s)?;
    let cons = c.child(s, "consequence")?;
    let x = else_body(c, s)?;
    let ind = indent_of(c, s);
    let xt = if c.kind(x) == "compound_statement" { inner_block(c, x) } else { c.text(x).to_string() };
    let xt = if ends_with_return(c, x) { xt } else { format!("{xt}\n{ind}    return;") };
    let has_decl = c.kind(cons) == "compound_statement" && stmts_of(c, cons).iter().any(|&t| c.kind(t) == "declaration");
    let r = if has_decl || c.kind(cons) != "compound_statement" { c.text(cons).to_string() } else { inner_block(c, cons) };
    let parts = [format!("if ({}) {{\n{ind}    {xt}\n{ind}}}", negate(c, ce)), r];
    Some(vec![replace_stmt_many(c, s, &parts)])
}

// ------------------------------------------------------------------ increment folded into its test

/// `x++;` / `++x;` / `x += 1;` / `x = x + 1;` (and decrements) of a pure lvalue: (lvalue, `++`/`--`).
fn step_of(c: &Cst, s: usize) -> Option<(usize, &'static str)> {
    if c.kind(s) != "expression_statement" {
        return None;
    }
    let e = unparen(c, *c.named(s).first()?);
    let one = |n: usize| c.text(n).trim() == "1";
    let (lv, inc) = match c.kind(e) {
        "update_expression" => (c.child(e, "argument")?, c.op(e)? == "++"),
        "assignment_expression" => {
            let (l, r) = (c.child(e, "left")?, c.child(e, "right")?);
            match c.op(e)? {
                "+=" if one(r) => (l, true),
                "-=" if one(r) => (l, false),
                "=" if c.kind(r) == "binary_expression" && matches!(c.op(r), Some("+") | Some("-")) => {
                    let (rl, rr) = (c.child(r, "left")?, c.child(r, "right")?);
                    if normalize(c.text(rl)) != normalize(c.text(l)) || !one(rr) {
                        return None;
                    }
                    (l, c.op(r) == Some("+"))
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    (pure(c, lv) && matches!(c.kind(lv), "identifier" | "field_expression")).then_some((lv, if inc { "++" } else { "--" }))
}

/// The comparison of an if condition and the side holding exactly `x` (the other not mentioning it).
fn compare_side(c: &Cst, s: usize, x: &str) -> Option<usize> {
    let ce = unparen(c, cond_expr(c, s)?);
    if c.kind(ce) != "binary_expression" || !matches!(c.op(ce), Some("==" | "!=" | "<" | ">" | "<=" | ">=")) {
        return None;
    }
    let (l, r) = (c.child(ce, "left")?, c.child(ce, "right")?);
    let root: String = x.chars().take_while(|ch| ch.is_alphanumeric() || *ch == '_').collect();
    for (a, b) in [(l, r), (r, l)] {
        if normalize(c.text(unparen(c, a))) == x && pure(c, b) && !mentions(c, b, &root) {
            return Some(unparen(c, a));
        }
    }
    None
}

/// `++x; if (x == n)` <-> `if (++x == n)` (and `--x == 0`): the updated value is compared
/// straight from the add (`addic.` / `cmpwi` on the new value).
pub fn op_incr_test(m: &mut M) -> Option<Vec<Edit>> {
    let c = m.cst;
    let mut fwd = Vec::new();
    for (_, l) in m.lists(2) {
        for w in l.windows(2) {
            if c.kind(w[1]) != "if_statement" || !m.nodes.contains(&w[0]) {
                continue;
            }
            let Some((lv, op)) = step_of(c, w[0]) else { continue };
            if let Some(side) = compare_side(c, w[1], &normalize(c.text(lv))) {
                fwd.push((w[0], side, format!("{op}{}", c.text(lv))));
            }
        }
    }
    let mut back = Vec::new();
    for s in m.of_kind(&["if_statement"]) {
        let Some(ce) = cond_expr(c, s).map(|e| unparen(c, e)) else { continue };
        if c.kind(ce) != "binary_expression" {
            continue;
        }
        for side in [c.child(ce, "left"), c.child(ce, "right")].into_iter().flatten() {
            let u = unparen(c, side);
            if c.kind(u) != "update_expression" || !c.text(u).starts_with(['+', '-']) {
                continue;
            }
            let Some(a) = c.child(u, "argument").filter(|&a| pure(c, a)) else { continue };
            if compare_side(c, s, &normalize(c.text(a))).is_none() && c.descendants(ce).into_iter().filter(|&d| c.kind(d) == "update_expression").count() == 1 {
                back.push((s, u, a));
            }
        }
    }
    if !fwd.is_empty() && (back.is_empty() || m.rng.chance(0.5)) {
        let (st, side, t) = m.pick_one(&fwd)?;
        return Some(vec![remove_stmt(c, st), Edit::replace(c, side, t)]);
    }
    let (s, u, a) = m.pick(&back)?;
    let ind = indent_of(c, s);
    let op = c.op(u)?;
    let cond = text_with(c, s, u, c.text(a));
    if c.parent(s).is_some_and(|p| c.kind(p) == "compound_statement") {
        Some(vec![Edit::replace(c, s, format!("{op}{};\n{ind}{cond}", c.text(a)))])
    } else {
        Some(vec![replace_stmt_many(c, s, &[format!("{op}{};", c.text(a)), cond])])
    }
}

// ------------------------------------------------------------------ function-local literal constants

/// Float literal (decimal point or exponent, not hex).
fn is_float_lit(c: &Cst, n: usize) -> bool {
    let t = c.text(n).to_ascii_lowercase();
    c.kind(n) == "number_literal" && !t.starts_with("0x") && (t.contains('.') || t.contains('e'))
}

/// The function's first statement (where a local constant goes) and its indentation.
fn body_top(c: &Cst, body: usize) -> (usize, String) {
    match stmts_of(c, body).first() {
        Some(&s) => (c.nodes[s].start, indent_of(c, s)),
        None => (c.nodes[body].start + 1, "    ".into()),
    }
}

/// A float literal used in an expression -> `static const float kN = <lit>;` at the top of the
/// function, used in its place (a constant object of its own: loaded from its own slot, not
/// shared with equal literals); and back (such a constant inlined into its uses).
pub fn op_static_literal(m: &mut M) -> Option<Vec<Edit>> {
    let c = m.cst;
    let in_static = |n: usize| c.ancestors(n).into_iter().any(|a| c.kind(a) == "declaration" && c.text(a).trim_start().starts_with("static")) || c.ancestors(n).into_iter().any(|a| c.kind(a) == "case_statement" && c.child(a, "value").is_some_and(|v| c.contains(v, n)));
    let lits: Vec<usize> = m.of_kind(&["number_literal"]).into_iter().filter(|&n| is_float_lit(c, n) && !in_static(n)).collect();
    // `static const float k = 1.5f;` at the top of the body, read only
    let mut consts = Vec::new();
    for s in stmts_of(c, m.info.body) {
        if c.kind(s) != "declaration" || !c.text(s).trim_start().starts_with("static const") {
            continue;
        }
        let ds: Vec<usize> = c.children_by_field(s, "declarator").collect();
        let [d] = ds.as_slice() else { continue };
        let Some(v) = c.child(*d, "value").filter(|&v| is_float_lit(c, v)) else { continue };
        let Some((name, suf)) = func::declarator_name(c, *d) else { continue };
        if suf.is_empty() && !uses_of(c, m.info.body, &name).is_empty() {
            consts.push((s, v, name));
        }
    }
    if !lits.is_empty() && (consts.is_empty() || m.rng.chance(0.6)) {
        let n = m.pick(&lits)?;
        let t = c.text(n).to_string();
        let ty = if t.ends_with(['f', 'F']) { "float" } else { "double" };
        let name = m.info.fresh_name(c, "kLit");
        let (at, ind) = body_top(c, m.info.body);
        let mut e = vec![Edit::insert(at, format!("static const {ty} {name} = {t};\n{ind}"))];
        if m.rng.chance(0.5) {
            // every equal literal of the body
            e.extend(c.descendants(m.info.body).into_iter().filter(|&x| c.kind(x) == "number_literal" && c.text(x) == t && !in_static(x)).map(|x| Edit::replace(c, x, name.clone())));
        } else {
            e.push(Edit::replace(c, n, name));
        }
        return Some(e);
    }
    let (s, v, name) = m.pick_one(&consts)?;
    let mut e: Vec<Edit> = uses_of(c, m.info.body, &name).into_iter().filter(|&u| !c.contains(s, u)).map(|u| Edit::replace(c, u, c.text(v))).collect();
    e.push(remove_stmt(c, s));
    Some(e)
}

// ------------------------------------------------------------------ vector component forms

/// `A.GetC()` / `A.SetC(..)` call: (call, receiver, component, Get?).
fn accessor(c: &Cst, n: usize) -> Option<(usize, usize, bool)> {
    if c.kind(n) != "call_expression" {
        return None;
    }
    let f = c.child(n, "function").filter(|&f| c.kind(f) == "field_expression" && c.op(f) == Some("."))?;
    let name = c.text(c.child(f, "field")?);
    let (get, comp) = match name {
        "GetX" => (true, 0),
        "GetY" => (true, 1),
        "GetZ" => (true, 2),
        "SetX" => (false, 0),
        "SetY" => (false, 1),
        "SetZ" => (false, 2),
        _ => return None,
    };
    let nargs = c.named(c.child(n, "arguments")?).len();
    ((get && nargs == 0) || (!get && nargs == 1)).then_some((c.child(f, "argument")?, comp, get))
}

/// `A[k]` with `k` one of the context's component index constants: (receiver, component).
fn indexed(m: &M, n: usize) -> Option<(usize, usize)> {
    let c = m.cst;
    let dims = m.ctx?.dims.as_ref()?;
    if c.kind(n) != "subscript_expression" {
        return None;
    }
    let a = c.child(n, "argument")?;
    let idx = c.child(n, "index").or_else(|| c.named(n).get(1).copied())?;
    let idx = if c.kind(idx) == "subscript_argument_list" { *c.named(idx).first()? } else { idx };
    let k = dims.iter().position(|d| normalize(d) == normalize(c.text(idx)))?;
    Some((a, k))
}

/// Component access forms of 3-vectors: `v.GetX()` <-> `v[kDX]`, `v.SetZ(e);` <-> `v[kDZ] = e;`
/// and `v.SetZ(v.GetZ() + h);` <-> `v[kDZ] += h;` (the index constants are the single-value enums
/// the context's `operator[]` overloads take; no site without them).
pub fn op_vec_index(m: &mut M) -> Option<Vec<Edit>> {
    let c = m.cst;
    let dims = m.ctx?.dims.clone()?;
    let mut cands: Vec<(usize, String)> = Vec::new();
    for n in m.nodes.clone() {
        if let Some((recv, k, get)) = accessor(c, n) {
            let r = ptext(c, recv);
            if get {
                if c.parent(n).is_some_and(|p| c.kind(p) == "field_expression") {
                    continue;
                }
                cands.push((n, format!("{r}[{}]", dims[k])));
            } else if c.parent(n).is_some_and(|p| c.kind(p) == "expression_statement") {
                let arg = *c.named(c.child(n, "arguments")?).first()?;
                let a = unparen(c, arg);
                // `v.SetC(v.GetC() op h)` -> `v[kDC] op= h`
                let compound = (c.kind(a) == "binary_expression" && matches!(c.op(a), Some("+" | "-" | "*" | "/")))
                    .then(|| (c.child(a, "left"), c.child(a, "right"), c.op(a)))
                    .and_then(|(l, rhs, op)| {
                        let (l, rr) = (l?, rhs?);
                        let (lr, lk, lg) = accessor(c, unparen(c, l))?;
                        (lg && lk == k && normalize(c.text(lr)) == normalize(c.text(recv))).then(|| format!("{r}[{}] {}= {}", dims[k], op.unwrap_or("+"), c.text(rr)))
                    });
                cands.push((n, compound.unwrap_or_else(|| format!("{r}[{}] = {}", dims[k], c.text(arg)))));
            }
            continue;
        }
        let Some((recv, k)) = indexed(m, n) else { continue };
        let r = ptext(c, recv);
        let p = c.parent(n);
        match p.filter(|&p| c.kind(p) == "assignment_expression" && c.child(p, "left") == Some(n)) {
            Some(a) => {
                if c.parent(a).is_none_or(|s| c.kind(s) != "expression_statement") {
                    continue;
                }
                let rhs = c.child(a, "right")?;
                let set = ["SetX", "SetY", "SetZ"][k];
                let t = match c.op(a)? {
                    "=" => format!("{r}.{set}({})", c.text(rhs)),
                    op if matches!(op, "+=" | "-=" | "*=" | "/=") => format!("{r}.{set}({r}.{}() {} {})", ["GetX", "GetY", "GetZ"][k], &op[..1], ptext_bin(c, rhs)),
                    _ => continue,
                };
                cands.push((a, t));
            }
            None => {
                if p.is_some_and(|p| matches!(c.kind(p), "update_expression" | "pointer_expression") || (c.kind(p) == "assignment_expression")) {
                    continue;
                }
                cands.push((n, format!("{r}.{}()", ["GetX", "GetY", "GetZ"][k])));
            }
        }
    }
    let (n, t) = m.pick_one(&cands)?;
    Some(vec![Edit::replace(c, n, t)])
}

/// Declared class of local/parameter `name` (without `const`, `&`).
fn class_of(m: &M, name: &str) -> Option<String> {
    let v = m.info.vars.get(name)?;
    let t = v.ty.trim().trim_start_matches("const ").trim_end_matches('&').trim().trim_end_matches("const").trim();
    (!t.is_empty() && !t.ends_with('*') && !is_scalar_type(t) && !t.contains("__typeof__")).then(|| t.to_string())
}

/// `v.MagSquared()` <-> `T::Dot(v, v)` for a local or parameter `v` of class `T`.
pub fn op_vec_dot(m: &mut M) -> Option<Vec<Edit>> {
    let c = m.cst;
    let mut cands: Vec<(usize, String)> = Vec::new();
    for n in m.of_kind(&["call_expression"]) {
        let (Some(f), Some(args)) = (c.child(n, "function"), c.child(n, "arguments")) else { continue };
        let args = c.named(args);
        match c.kind(f) {
            "field_expression" if c.op(f) == Some(".") && args.is_empty() => {
                if c.child(f, "field").is_none_or(|x| c.text(x) != "MagSquared") {
                    continue;
                }
                let Some(r) = c.child(f, "argument").filter(|&r| c.kind(r) == "identifier") else { continue };
                if let Some(t) = class_of(m, c.text(r)) {
                    cands.push((n, format!("{t}::Dot({0}, {0})", c.text(r))));
                }
            }
            "qualified_identifier" if args.len() == 2 => {
                if c.child(f, "name").is_none_or(|x| c.text(x) != "Dot") {
                    continue;
                }
                if normalize(c.text(args[0])) == normalize(c.text(args[1])) && pure(c, args[0]) {
                    cands.push((n, format!("{}.MagSquared()", ptext(c, args[0]))));
                }
            }
            _ => {}
        }
    }
    let (n, t) = m.pick_one(&cands)?;
    Some(vec![Edit::replace(c, n, t)])
}

/// `T p(a.GetX(), a.GetY(), z);` <-> `T p = a; p.SetZ(z);` (or `p[kDZ] = z;`): a copy then one
/// component store instead of building from components (a struct copy, then one store).
pub fn op_vec_copy(m: &mut M) -> Option<Vec<Edit>> {
    let c = m.cst;
    let dims = m.ctx.and_then(|x| x.dims.clone());
    let mut fwd = Vec::new();
    let mut back = Vec::new();
    for (_, l) in m.lists(1) {
        for (i, &s) in l.iter().enumerate() {
            if c.kind(s) != "declaration" || !m.nodes.contains(&s) {
                continue;
            }
            let ds: Vec<usize> = c.children_by_field(s, "declarator").collect();
            let [d] = ds.as_slice() else { continue };
            let Some((name, suf)) = func::declarator_name(c, *d) else { continue };
            let Some(ty) = c.child(s, "type").map(|t| c.text(t).to_string()) else { continue };
            if !suf.is_empty() || is_scalar_type(&ty) {
                continue;
            }
            // T p(a, b, c) / T p = T(a, b, c)
            let args = match c.kind(*d) {
                "init_declarator" => c.child(*d, "value").and_then(|v| match c.kind(v) {
                    "argument_list" => Some(v),
                    "call_expression" => c.child(v, "arguments"),
                    _ => None,
                }),
                _ => None,
            };
            if let Some(al) = args {
                let a = c.named(al);
                if a.len() == 3 {
                    let comps: Vec<Option<(String, usize)>> = a.iter().map(|&x| accessor(c, unparen(c, x)).filter(|t| t.2).map(|(r, k, _)| (normalize(c.text(r)), k))).collect();
                    for k in 0..3 {
                        let others: Vec<&(String, usize)> = (0..3).filter(|&j| j != k).filter_map(|j| comps[j].as_ref().filter(|x| x.1 == j)).collect();
                        if others.len() == 2 && others[0].0 == others[1].0 && comps[k].as_ref().is_none_or(|x| x.0 != others[0].0 || x.1 != k) && !mentions(c, a[k], &name) {
                            let recv = accessor(c, unparen(c, a[(k + 1) % 3])).map(|t| t.0)?;
                            if pure(c, recv) {
                                fwd.push((s, ty.clone(), name.clone(), recv, k, a[k]));
                            }
                        }
                    }
                }
                continue;
            }
            // T p = a; followed by p.SetC(e); / p[kDC] = e;
            let Some(v) = c.child(*d, "value").filter(|&v| pure(c, v)) else { continue };
            let Some(&next) = l.get(i + 1) else { continue };
            let Some(e) = c.named(next).first().copied().filter(|_| c.kind(next) == "expression_statement") else { continue };
            let set = match accessor(c, e) {
                Some((r, k, false)) if c.text(r) == name => c.named(c.child(e, "arguments")?).first().map(|&x| (k, x)),
                _ => (c.kind(e) == "assignment_expression" && c.op(e) == Some("="))
                    .then(|| c.child(e, "left").and_then(|l| indexed(m, l)).filter(|(r, _)| c.text(*r) == name).and_then(|(_, k)| c.child(e, "right").map(|x| (k, x))))
                    .flatten(),
            };
            if let Some((k, x)) = set.filter(|(_, x)| !mentions(c, *x, &name)) {
                back.push((s, next, ty.clone(), name.clone(), v, k, x));
            }
        }
    }
    if !fwd.is_empty() && (back.is_empty() || m.rng.chance(0.5)) {
        let (s, ty, name, recv, k, val) = m.pick_one(&fwd)?;
        let ind = indent_of(c, s);
        let store = match (&dims, m.rng.chance(0.5)) {
            (Some(d), true) => format!("{name}[{}] = {};", d[k], c.text(val)),
            _ => format!("{name}.{}({});", ["SetX", "SetY", "SetZ"][k], c.text(val)),
        };
        return Some(vec![Edit::replace(c, s, format!("{ty} {name} = {};\n{ind}{store}", c.text(recv)))]);
    }
    let (s, next, ty, name, v, k, x) = m.pick_one(&back)?;
    let a = ptext(c, v);
    let args: Vec<String> = (0..3).map(|j| if j == k { c.text(x).to_string() } else { format!("{a}.{}()", ["GetX", "GetY", "GetZ"][j]) }).collect();
    Some(vec![Edit { start: c.nodes[s].start, end: c.nodes[next].end, text: format!("{ty} {name}({});", args.join(", ")) }])
}

// ------------------------------------------------------------------ float view of a vector local

/// `const float* v = (const float*)&local;` (or `reinterpret_cast`): (view, local).
fn float_view(c: &Cst, s: usize) -> Option<(String, String)> {
    if c.kind(s) != "declaration" || !c.text(s).trim_start().starts_with("const float") {
        return None;
    }
    let ds: Vec<usize> = c.children_by_field(s, "declarator").collect();
    let [d] = ds.as_slice() else { return None };
    let (name, suf) = func::declarator_name(c, *d)?;
    let v = c.child(*d, "value")?;
    if suf != "*" {
        return None;
    }
    let addr = c.descendants(v).into_iter().find(|&x| c.kind(x) == "pointer_expression" && c.op(x) == Some("&"))?;
    let a = c.child(addr, "argument").filter(|&a| c.kind(a) == "identifier")?;
    let t = c.text(v);
    (t.starts_with("(const float*)") || t.starts_with("(const float *)") || t.starts_with("reinterpret_cast")).then(|| (name, c.text(a).to_string()))
}

/// A 3-float vector local (or by-value parameter) whose components are passed to calls read
/// through a float pointer view instead: `const float* v = (const float*)&local;` after its
/// definition and `v[0]`, `v[1]`, `v[2]` for `local.GetX()` ... (the object stays in memory
/// rather than in registers); and back.
pub fn op_float_view(m: &mut M) -> Option<Vec<Edit>> {
    let c = m.cst;
    let body = m.info.body;
    let mut fwd = Vec::new();
    let mut names: Vec<&String> = m.info.vars.keys().collect();
    names.sort();
    for name in names {
        let Some(cls) = class_of(m, name) else { continue };
        if m.ctx.and_then(|x| x.vec3.get(&cls)).is_some_and(|ok| !ok) || m.info.aliased.contains(name.as_str()) {
            continue;
        }
        let var = &m.info.vars[name];
        // after the local's declaration (in its block), or at the top for a parameter
        let (at, ind, scope) = if var.is_param {
            let (at, ind) = body_top(c, body);
            (at, ind, body)
        } else {
            let d = var.decl;
            if c.parent(d).is_none_or(|p| c.kind(p) != "compound_statement") || c.children_by_field(d, "declarator").count() != 1 {
                continue;
            }
            let after = c.nodes[d].end;
            (after, format!("\n{}", indent_of(c, d)), c.parent(d).unwrap())
        };
        let gets: Vec<(usize, usize, bool)> = uses_of(c, scope, name)
            .into_iter()
            .filter(|&u| c.nodes[u].start >= at)
            .filter_map(|u| {
                let f = c.parent(u).filter(|&f| c.kind(f) == "field_expression")?;
                let call = c.parent(f)?;
                let (_, k, get) = accessor(c, call)?;
                let in_arg = c.parent(call).is_some_and(|p| c.kind(p) == "argument_list");
                get.then_some((call, k, in_arg))
            })
            .collect();
        if gets.iter().any(|g| g.2 && m.nodes.contains(&g.0)) {
            fwd.push((name.clone(), at, ind, gets));
        }
    }
    let mut back = Vec::new();
    for s in c.descendants(body) {
        let Some((view, local)) = float_view(c, s) else { continue };
        let Some(scope) = c.parent(s) else { continue };
        let uses = uses_of(c, scope, &view);
        let subs: Option<Vec<(usize, usize)>> = uses
            .iter()
            .map(|&u| {
                let p = c.parent(u).filter(|&p| c.kind(p) == "subscript_expression")?;
                let k: usize = c.text(p).rsplit('[').next()?.trim_end_matches(']').trim().parse().ok()?;
                (k < 3 && !c.parent(p).is_some_and(|q| c.kind(q) == "assignment_expression" && c.child(q, "left") == Some(p))).then_some((p, k))
            })
            .collect();
        if let Some(subs) = subs.filter(|v| !v.is_empty()) {
            back.push((s, local, subs));
        }
    }
    if !fwd.is_empty() && (back.is_empty() || m.rng.chance(0.6)) {
        let (name, at, ind, gets) = m.pick_one(&fwd)?;
        let view = m.info.fresh_name(c, "fv");
        let decl = format!("const float* {view} = (const float*)&{name};");
        let mut e = vec![Edit::insert(at, if ind.starts_with('\n') { format!("{ind}{decl}") } else { format!("{decl}\n{ind}") })];
        let only_args = m.rng.chance(0.5);
        e.extend(gets.iter().filter(|g| g.2 || !only_args).map(|&(call, k, _)| Edit::replace(c, call, format!("{view}[{k}]"))));
        return Some(e);
    }
    let (s, local, subs) = m.pick_one(&back)?;
    let mut e: Vec<Edit> = subs.iter().map(|&(p, k)| Edit::replace(c, p, format!("{local}.{}()", ["GetX", "GetY", "GetZ"][k]))).collect();
    e.push(remove_stmt(c, s));
    Some(e)
}

// ------------------------------------------------------------------ scopes of objects with destructors

/// A declaration of a class-type object (non-scalar, not a pointer or reference): its scope
/// decides when its destructor runs and which stack slot it shares.
pub(crate) fn declares_object(c: &Cst, s: usize) -> bool {
    if c.kind(s) != "declaration" || c.text(s).trim_start().starts_with("static") {
        return false;
    }
    let Some(t) = c.child(s, "type") else { return false };
    !is_scalar_type(c.text(t)) && c.children_by_field(s, "declarator").any(|d| func::declarator_name(c, d).is_some_and(|(_, suf)| suf.is_empty()))
}

/// Names declared directly in block statements `l`.
fn declared_in(c: &Cst, l: &[usize]) -> Vec<String> {
    l.iter().filter(|&&s| c.kind(s) == "declaration").flat_map(|&s| names_declared(c, s)).collect()
}

/// Scope of class-type locals: unwrap a block declaring one (`{ T t; ... }` -> `T t; ...`, its
/// destructor moves to the end of the enclosing scope) or wrap a run starting at such a
/// declaration in a block (the destructor runs at the block's end). `unwrap_block`/`wrap_block`
/// do the same for any block at a near-zero prior: for these blocks the scope is codegen.
pub fn op_object_scope(m: &mut M) -> Option<Vec<Edit>> {
    let c = m.cst;
    let all_decls: Vec<String> = c.descendants(m.info.body).into_iter().filter(|&d| c.kind(d) == "declaration").flat_map(|d| names_declared(c, d)).collect();
    let mut unwrap = Vec::new();
    for b in m.of_kind(&["compound_statement"]) {
        if b == m.info.body || c.parent(b).is_none_or(|p| c.kind(p) != "compound_statement") {
            continue;
        }
        let l = stmts_of(c, b);
        if !l.iter().any(|&s| declares_object(c, s)) {
            continue;
        }
        if declared_in(c, &l).iter().all(|n| all_decls.iter().filter(|x| *x == n).count() == 1 && m.info.vars.get(n).is_some_and(|v| !v.is_param)) {
            unwrap.push(b);
        }
    }
    let mut wrap = Vec::new();
    for (_, l) in m.lists(1) {
        for (i, &s) in l.iter().enumerate() {
            if !declares_object(c, s) || !m.nodes.contains(&s) {
                continue;
            }
            for j in i..l.len() {
                let names = declared_in(c, &l[i..=j]);
                if l[j + 1..].iter().any(|&t| names.iter().any(|n| mentions(c, t, n))) || c.kind(l[j]) == "labeled_statement" {
                    continue;
                }
                // (a run that ends the block changes nothing)
                if j + 1 < l.len() || i > 0 {
                    wrap.push((l[i], l[j]));
                }
            }
        }
    }
    if !unwrap.is_empty() && (wrap.is_empty() || m.rng.chance(0.5)) {
        let b = m.pick(&unwrap)?;
        return Some(vec![Edit::replace(c, b, inner_block(c, b))]);
    }
    let (a, b) = m.pick(&wrap)?;
    let ind = indent_of(c, a);
    let body = &c.src[c.nodes[a].start..c.nodes[b].end];
    Some(vec![Edit { start: c.nodes[a].start, end: c.nodes[b].end, text: format!("{{\n{ind}    {body}\n{ind}}}") }])
}
