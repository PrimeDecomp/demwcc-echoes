//! Per-function analysis for the permuter: which function to mutate, local/param types, a
//! cheap side-effect model for statement reordering, and best-effort expression types.
use crate::cst::Cst;
use std::collections::{HashMap, HashSet};

pub const STMT_KINDS: &[&str] = &[
    "expression_statement",
    "declaration",
    "if_statement",
    "for_statement",
    "while_statement",
    "do_statement",
    "return_statement",
    "compound_statement",
    "switch_statement",
    "break_statement",
    "continue_statement",
    "goto_statement",
    "labeled_statement",
];

pub fn is_stmt(k: &str) -> bool {
    STMT_KINDS.contains(&k)
}

pub fn is_loop(k: &str) -> bool {
    matches!(k, "for_statement" | "while_statement" | "do_statement")
}

/// Declared name of a declarator (innermost identifier) and the pointer/reference suffix.
pub fn declarator_name(cst: &Cst, d: usize) -> Option<(String, String)> {
    let mut suffix = String::new();
    let mut n = d;
    loop {
        match cst.kind(n) {
            "identifier" | "field_identifier" => return Some((cst.text(n).to_string(), suffix)),
            "init_declarator" => n = cst.child(n, "declarator")?,
            "pointer_declarator" => {
                suffix.push('*');
                // `T* const p`
                n = cst.child(n, "declarator")?;
            }
            "reference_declarator" => {
                suffix.push('&');
                n = *cst.named(n).last()?;
            }
            "array_declarator" => {
                suffix.push_str("[]");
                n = cst.child(n, "declarator")?;
            }
            "parenthesized_declarator" => n = *cst.named(n).first()?,
            _ => return None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Var {
    pub name: String,
    /// Full type text, e.g. `const CVector3f&`, `u32*`, `int`.
    pub ty: String,
    pub is_param: bool,
    /// Declaration node (locals) or parameter_declaration.
    pub decl: usize,
}

/// Analysis of the function being permuted.
#[derive(Clone, Debug)]
pub struct FuncInfo {
    pub def: usize,
    pub body: usize,
    pub ret_ty: String,
    pub vars: HashMap<String, Var>,
    /// Locals whose address is taken (`&x`) or that are references/arrays: treat as memory.
    pub aliased: HashSet<String>,
}

/// Mangled-name base: `Foo__3BarFi` -> `Foo`, `__ct__3BarFv` -> `__ct`.
fn mangled_base(sym: &str) -> &str {
    let s = sym.strip_prefix("__").map(|r| (r, 2)).unwrap_or((sym, 0));
    match s.0.find("__") {
        Some(i) => &sym[..i + s.1],
        None => sym,
    }
}

/// Last component of a function definition's declarator name, e.g. `Bar` for `CFoo::Bar`,
/// `~CFoo` for a destructor, and the class name for `CFoo::CFoo`.
pub fn def_name(cst: &Cst, def: usize) -> Option<(String, Option<String>)> {
    let mut d = cst.child(def, "declarator")?;
    loop {
        match cst.kind(d) {
            "function_declarator" => d = cst.child(d, "declarator")?,
            "pointer_declarator" | "reference_declarator" => d = *cst.named(d).last()?,
            "qualified_identifier" => {
                let scope = cst.child(d, "scope").map(|s| cst.text(s).to_string());
                let name = cst.child(d, "name")?;
                if cst.kind(name) == "qualified_identifier" {
                    d = name;
                    continue;
                }
                return Some((cst.text(name).to_string(), scope));
            }
            _ => return Some((cst.text(d).to_string(), None)),
        }
    }
}

/// All function definitions with a body in the file.
pub fn function_defs(cst: &Cst) -> Vec<usize> {
    (0..cst.nodes.len()).filter(|&i| cst.kind(i) == "function_definition" && cst.child(i, "body").is_some()).collect()
}

/// The function definition to permute for mangled `symbol`: among the definitions whose name
/// matches the mangled base name (constructors/destructors by class name), those of the mangled
/// class, then those with the mangled parameter count; the last of the narrowest non-empty set.
/// With no name match, the last definition.
///
/// An init file can hold several constructors (inline sub-object constructors kept for a
/// constructor target, the target's own overloads), so the class and the parameter count matter.
pub fn find_target(cst: &Cst, symbol: &str) -> Option<usize> {
    let defs = function_defs(cst);
    let base = mangled_base(symbol);
    let sig = MangledSig::parse(symbol);
    let named: Vec<(usize, String, Option<String>)> = defs
        .iter()
        .copied()
        .filter_map(|d| {
            let (name, scope) = def_name(cst, d)?;
            let scope_last = scope.as_deref().map(|s| s.rsplit("::").next().unwrap_or(s).to_string());
            let ok = match base {
                "__ct" => scope_last.as_deref() == Some(name.as_str()),
                "__dt" => name.starts_with('~'),
                b if b.starts_with("__") => name.starts_with("operator") || name == b,
                b => name == b,
            };
            ok.then_some((d, name, scope))
        })
        .collect();
    let mut pick: Vec<usize> = named.iter().map(|x| x.0).collect();
    if let Some(sig) = &sig {
        if let Some(class) = &sig.class {
            let same: Vec<usize> =
                named.iter().filter(|(_, _, scope)| scope.as_deref().is_some_and(|s| scope_matches(s, class))).map(|x| x.0).collect();
            if !same.is_empty() {
                pick = same;
            }
        }
        if let Some(n) = sig.params {
            let arity: Vec<usize> = pick.iter().copied().filter(|&d| def_param_count(cst, d) == Some(n)).collect();
            if !arity.is_empty() {
                pick = arity;
            }
        }
    }
    pick.last().copied().or_else(|| defs.last().copied())
}

/// Does a definition's scope text (`CSandworm::SEyeEffects`, maybe inside a namespace block)
/// name `class` (the mangled qualified class, `CSandworm::SEyeEffects`)? The scope may omit
/// leading namespaces, so it must be a `::`-aligned suffix.
fn scope_matches(scope: &str, class: &str) -> bool {
    let norm = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let (s, c) = (norm(scope), norm(class));
    let s = s.trim_start_matches("::");
    // template arguments are spelled differently in source and mangled names (and may hold `::`):
    // drop them, then compare the names
    let bare = |x: &str| {
        let (mut out, mut depth) = (String::new(), 0usize);
        for ch in x.chars() {
            match ch {
                '<' => depth += 1,
                '>' => depth = depth.saturating_sub(1),
                _ if depth == 0 => out.push(ch),
                _ => {}
            }
        }
        out.split("::").map(String::from).collect::<Vec<_>>()
    };
    let (s, c) = (bare(s), bare(&c));
    s.len() <= c.len() && c[c.len() - s.len()..] == s[..]
}

/// Number of parameters of a function definition (`(void)` is none); `None` with `...`.
fn def_param_count(cst: &Cst, def: usize) -> Option<usize> {
    let fd = cst.descendants(cst.child(def, "declarator")?).into_iter().find(|&n| cst.kind(n) == "function_declarator")?;
    let pl = cst.child(fd, "parameters")?;
    let mut n = 0;
    for p in cst.named(pl) {
        match cst.kind(p) {
            "parameter_declaration" | "optional_parameter_declaration" => {
                if cst.child(p, "declarator").is_none() && cst.text(p).trim() == "void" {
                    continue;
                }
                n += 1;
            }
            "variadic_parameter_declaration" => return None,
            _ => {}
        }
    }
    if cst.text(pl).contains("...") {
        return None;
    }
    Some(n)
}

/// What a CodeWarrior mangled name says about the function's class and parameters.
#[derive(Debug, PartialEq)]
struct MangledSig {
    /// Qualified class, e.g. `CSandworm::SEyeEffects` (`None` for a free function).
    class: Option<String>,
    /// Parameter count (`None` when the signature can't be read, or is variadic).
    params: Option<usize>,
}

impl MangledSig {
    fn parse(sym: &str) -> Option<MangledSig> {
        let base = mangled_base(sym);
        let rest = sym.get(base.len()..)?.strip_prefix("__")?;
        let b = rest.as_bytes();
        let mut i = 0;
        let class = match b.first()? {
            b'Q' | b'0'..=b'9' => Some(read_qualified(rest, &mut i)?),
            _ => None,
        };
        if b.get(i) == Some(&b'C') {
            i += 1;
        }
        if b.get(i) != Some(&b'F') {
            return Some(MangledSig { class, params: None });
        }
        i += 1;
        Some(MangledSig { class, params: count_params(rest, &mut i, false) })
    }
}

/// `9CSandworm` or `Q29CSandworm10SEyeEffect` -> `CSandworm` / `CSandworm::SEyeEffect`.
fn read_qualified(s: &str, i: &mut usize) -> Option<String> {
    let b = s.as_bytes();
    if b.get(*i) == Some(&b'Q') {
        let n = (*b.get(*i + 1)? as char).to_digit(10)? as usize;
        *i += 2;
        let mut parts = vec![];
        for _ in 0..n {
            parts.push(read_name(s, i)?);
        }
        Some(parts.join("::"))
    } else {
        read_name(s, i)
    }
}

fn read_name(s: &str, i: &mut usize) -> Option<String> {
    let b = s.as_bytes();
    let start = *i;
    while b.get(*i).is_some_and(|c| c.is_ascii_digit()) {
        *i += 1;
    }
    let len: usize = s.get(start..*i)?.parse().ok()?;
    let name = s.get(*i..*i + len)?.to_string();
    *i += len;
    Some(name)
}

/// Count parameter types up to the end (or `_` inside a function type).
fn count_params(s: &str, i: &mut usize, in_fn: bool) -> Option<usize> {
    let b = s.as_bytes();
    if b.get(*i) == Some(&b'v') && (*i + 1 == b.len() || (in_fn && b.get(*i + 1) == Some(&b'_'))) {
        *i += 1;
        return Some(0);
    }
    let mut n = 0;
    while *i < b.len() && !(in_fn && b[*i] == b'_') {
        if b[*i] == b'e' {
            return None;
        }
        skip_type(s, i)?;
        n += 1;
    }
    Some(n)
}

fn skip_type(s: &str, i: &mut usize) -> Option<()> {
    let b = s.as_bytes();
    loop {
        match *b.get(*i)? {
            b'C' | b'V' | b'P' | b'R' | b'U' | b'S' => *i += 1,
            b'Q' | b'0'..=b'9' => return read_qualified(s, i).map(|_| ()),
            b'F' => {
                *i += 1;
                count_params(s, i, true)?;
                if b.get(*i) != Some(&b'_') {
                    return None;
                }
                *i += 1;
                return skip_type(s, i);
            }
            b'A' => {
                *i += 1;
                while b.get(*i).is_some_and(|c| c.is_ascii_digit()) {
                    *i += 1;
                }
                if b.get(*i) != Some(&b'_') {
                    return None;
                }
                *i += 1;
                return skip_type(s, i);
            }
            b'M' => {
                *i += 1;
                read_qualified(s, i)?;
                return skip_type(s, i);
            }
            b'v' | b'b' | b'c' | b's' | b'i' | b'l' | b'x' | b'f' | b'd' | b'r' | b'w' => {
                *i += 1;
                return Some(());
            }
            _ => return None,
        }
    }
}

fn type_prefix(cst: &Cst, decl: usize, first_declarator: usize) -> String {
    let s = cst.nodes[decl].start;
    let e = cst.nodes[first_declarator].start;
    cst.src[s..e].split_whitespace().collect::<Vec<_>>().join(" ")
}

impl FuncInfo {
    pub fn analyze(cst: &Cst, def: usize) -> Option<FuncInfo> {
        let body = cst.child(def, "body")?;
        let mut vars = HashMap::new();
        let mut aliased = HashSet::new();
        let ret_ty = cst.child(def, "type").map(|t| cst.text(t).to_string()).unwrap_or_default();
        // Parameters.
        if let Some(fd) = cst.descendants(cst.child(def, "declarator")?).into_iter().find(|&n| cst.kind(n) == "function_declarator") {
            if let Some(pl) = cst.child(fd, "parameters") {
                for p in cst.named(pl) {
                    if cst.kind(p) != "parameter_declaration" {
                        continue;
                    }
                    let Some(d) = cst.child(p, "declarator") else { continue };
                    let Some((name, suf)) = declarator_name(cst, d) else { continue };
                    let ty = format!("{}{}", type_prefix(cst, p, d), suf);
                    vars.insert(name.clone(), Var { name, ty, is_param: true, decl: p });
                }
            }
        }
        for n in cst.descendants(body) {
            match cst.kind(n) {
                "declaration" => {
                    let ds: Vec<usize> = cst.children_by_field(n, "declarator").collect();
                    let Some(&first) = ds.first() else { continue };
                    let prefix = type_prefix(cst, n, first);
                    for d in ds {
                        if let Some((name, suf)) = declarator_name(cst, d) {
                            if suf.contains('&') || suf.contains('[') {
                                aliased.insert(name.clone());
                            }
                            vars.insert(name.clone(), Var { name, ty: format!("{prefix}{suf}"), is_param: false, decl: n });
                        }
                    }
                }
                "pointer_expression" if cst.op(n) == Some("&") => {
                    if let Some(a) = cst.child(n, "argument") {
                        let mut a = a;
                        while cst.kind(a) == "field_expression" && cst.op(a) == Some(".") {
                            a = cst.child(a, "argument").unwrap_or(a);
                            if cst.kind(a) != "field_expression" {
                                break;
                            }
                        }
                        if cst.kind(a) == "identifier" {
                            aliased.insert(cst.text(a).to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        Some(FuncInfo { def, body, ret_ty, vars, aliased })
    }

    /// Is `name` a non-aliased local or parameter (a pure register-like value)?
    pub fn is_private(&self, name: &str) -> bool {
        self.vars.contains_key(name) && !self.aliased.contains(name)
    }

    pub fn fresh_name(&self, cst: &Cst, stem: &str) -> String {
        for k in 0.. {
            let n = format!("{stem}{k}");
            if !self.vars.contains_key(&n) && !cst.src.contains(&n) {
                return n;
            }
        }
        unreachable!()
    }
}

/// What a piece of code reads/writes, conservatively.
#[derive(Clone, Debug, Default)]
pub struct Effects {
    pub reads: HashSet<String>,
    pub writes: HashSet<String>,
    pub mem_read: bool,
    pub mem_write: bool,
    /// return/break/continue/goto/labels/switch case labels: never reorder across.
    pub control: bool,
}

impl Effects {
    pub fn conflicts(&self, o: &Effects) -> bool {
        if self.control || o.control {
            return true;
        }
        if (self.mem_write && (o.mem_read || o.mem_write)) || (o.mem_write && self.mem_read) {
            return true;
        }
        self.writes.iter().any(|w| o.reads.contains(w) || o.writes.contains(w)) || o.writes.iter().any(|w| self.reads.contains(w))
    }
    pub fn merge(&mut self, o: &Effects) {
        self.reads.extend(o.reads.iter().cloned());
        self.writes.extend(o.writes.iter().cloned());
        self.mem_read |= o.mem_read;
        self.mem_write |= o.mem_write;
        self.control |= o.control;
    }
}

/// Root identifier of an lvalue if it is a private local accessed directly (`x`, `x.a.b`).
fn lvalue_local<'a>(cst: &'a Cst, info: &FuncInfo, mut n: usize) -> Option<&'a str> {
    loop {
        match cst.kind(n) {
            "identifier" => {
                let t = cst.text(n);
                return info.is_private(t).then_some(t);
            }
            "parenthesized_expression" => n = *cst.named(n).first()?,
            "field_expression" if cst.op(n) == Some(".") => n = cst.child(n, "argument")?,
            _ => return None,
        }
    }
}

pub fn effects(cst: &Cst, info: &FuncInfo, root: usize) -> Effects {
    let mut e = Effects::default();
    let mut st = vec![root];
    while let Some(n) = st.pop() {
        let k = cst.kind(n);
        match k {
            "return_statement" | "break_statement" | "continue_statement" | "goto_statement" | "labeled_statement"
            | "case_statement" | "throw_statement" => e.control = true,
            "call_expression" | "new_expression" | "delete_expression" => {
                e.mem_read = true;
                e.mem_write = true;
            }
            "assignment_expression" | "update_expression" => {
                let lhs = cst.child(n, if k == "update_expression" { "argument" } else { "left" });
                if let Some(l) = lhs {
                    match lvalue_local(cst, info, l) {
                        Some(v) => {
                            e.writes.insert(v.to_string());
                            if k == "update_expression" || cst.op(n) != Some("=") {
                                e.reads.insert(v.to_string());
                            }
                        }
                        None => e.mem_write = true,
                    }
                }
            }
            "field_expression" => {
                if cst.op(n) == Some("->") {
                    e.mem_read = true;
                }
            }
            "subscript_expression" => e.mem_read = true,
            "pointer_expression" => {
                if cst.op(n) == Some("*") {
                    e.mem_read = true;
                }
            }
            "this" => e.mem_read = true,
            "identifier" => {
                // Skip names in call position / qualified names / declarator names.
                let t = cst.text(n);
                let par = cst.parent(n);
                let in_decl = par.is_some_and(|p| {
                    let pk = cst.kind(p);
                    (pk == "init_declarator" && cst.nodes[n].field == Some("declarator"))
                        || (pk == "declaration" && cst.nodes[n].field == Some("declarator"))
                        || matches!(pk, "pointer_declarator" | "reference_declarator" | "array_declarator")
                });
                if in_decl {
                    e.writes.insert(t.to_string());
                } else if info.is_private(t) {
                    e.reads.insert(t.to_string());
                } else if par.is_some_and(|p| cst.kind(p) == "call_expression" && cst.nodes[n].field == Some("function")) {
                    // function name: covered by the call
                } else {
                    // Globals, members accessed implicitly, aliased locals: memory.
                    e.mem_read = true;
                    if info.vars.contains_key(t) {
                        e.reads.insert(t.to_string());
                    }
                }
            }
            _ => {}
        }
        // Writes to aliased locals are memory writes too (handled: lvalue_local None => mem_write).
        st.extend(cst.nodes[n].children.iter().copied());
    }
    // A declaration with a class type may run a constructor: treat as a call.
    if cst.kind(root) == "declaration" {
        if let Some(t) = cst.child(root, "type") {
            if !is_scalar_type(cst.text(t)) {
                e.mem_read = true;
                e.mem_write = true;
            }
        }
    }
    e
}

/// Zero-argument `Get*` accessor call (`a.GetX()`, `p->GetFoo()`): taken as a read of its
/// object, not as a call with side effects.
pub fn is_getter_call(cst: &Cst, n: usize) -> bool {
    cst.kind(n) == "call_expression"
        && cst.child(n, "arguments").is_some_and(|a| cst.named(a).is_empty())
        && cst.child(n, "function").is_some_and(|f| cst.kind(f) == "field_expression" && cst.child(f, "field").is_some_and(|x| cst.text(x).starts_with("Get")))
}

/// Builtin arithmetic / pointer types (and the project's common typedefs).
pub fn is_scalar_type(t: &str) -> bool {
    let t = t.trim();
    if t.ends_with('*') {
        return true;
    }
    let t = t.trim_start_matches("const ").trim_start_matches("volatile ").trim();
    matches!(
        t,
        "int" | "unsigned int" | "unsigned" | "signed int" | "short" | "unsigned short" | "signed short" | "char"
            | "unsigned char" | "signed char" | "long" | "unsigned long" | "long long" | "unsigned long long"
            | "float" | "double" | "bool" | "s8" | "u8" | "s16" | "u16" | "s32" | "u32" | "s64" | "u64" | "f32" | "f64"
            | "uint" | "ushort" | "uchar" | "size_t" | "BOOL" | "int8_t" | "uint8_t" | "int16_t" | "uint16_t"
            | "int32_t" | "uint32_t"
    )
}

pub fn is_int_type(t: &str) -> bool {
    is_scalar_type(t) && !t.ends_with('*') && !matches!(t.trim_start_matches("const ").trim(), "float" | "double" | "f32" | "f64")
}

/// Best-effort static type of an expression (text), or `None`.
pub fn infer_type(cst: &Cst, info: &FuncInfo, n: usize) -> Option<String> {
    match cst.kind(n) {
        "identifier" => info.vars.get(cst.text(n)).map(|v| v.ty.trim_end_matches('&').trim().trim_start_matches("const ").to_string()),
        "number_literal" => {
            let t = cst.text(n).to_ascii_lowercase();
            if t.starts_with("0x") {
                Some(if t.ends_with('u') { "unsigned int" } else { "int" }.into())
            } else if t.ends_with('f') {
                Some("float".into())
            } else if t.contains('.') || t.contains('e') {
                Some("double".into())
            } else if t.ends_with('u') {
                Some("unsigned int".into())
            } else {
                Some("int".into())
            }
        }
        "true" | "false" => Some("bool".into()),
        "cast_expression" => cst.child(n, "type").map(|t| cst.text(t).to_string()),
        "parenthesized_expression" => cst.named(n).first().and_then(|&c| infer_type(cst, info, c)),
        "binary_expression" => {
            let op = cst.op(n)?;
            if matches!(op, "<" | ">" | "<=" | ">=" | "==" | "!=" | "&&" | "||") {
                return Some("bool".into());
            }
            let l = infer_type(cst, info, cst.child(n, "left")?)?;
            let r = infer_type(cst, info, cst.child(n, "right")?)?;
            if l == r && (is_scalar_type(&l)) {
                let narrow = matches!(l.as_str(), "short" | "unsigned short" | "char" | "unsigned char" | "signed char" | "s8" | "u8" | "s16" | "u16" | "bool");
                if narrow { Some("int".into()) } else { Some(l) }
            } else if l.ends_with('*') && is_int_type(&r) && matches!(op, "+" | "-") {
                Some(l)
            } else {
                None
            }
        }
        "unary_expression" => {
            let op = cst.op(n)?;
            if op == "!" {
                return Some("bool".into());
            }
            infer_type(cst, info, cst.child(n, "argument")?)
        }
        _ => None,
    }
}

/// Type to declare a temporary holding expression `n`: inferred if possible, else `__typeof__`.
pub fn temp_type(cst: &Cst, info: &FuncInfo, n: usize) -> String {
    match infer_type(cst, info, n) {
        Some(t) if !t.is_empty() => t,
        _ => format!("__typeof__({})", cst.text(n)),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
inline CSandworm::SPincerData::SPincerData(int l, int r, float s) : mL(l), mR(r), mS(s) {}
CSandworm::CSandworm(int a, int b) : mA(a) { Load(b); }
CSandworm::CSandworm(int a, int b, float c) : mA(a), mMode(0) { Init(b, c); }
CSandworm::~CSandworm() {}
void CSandworm::Initialize() {}
CSandwormEye::CSandwormEye(int a) : mA(a) {}
CSandwormEye::~CSandwormEye() {}
CSandworm::SEyeEffects::SEyeEffects(int g, int e1, int e2) {}
CSandworm::SEyeEffects::~SEyeEffects() {}
"#;

    fn line_of(cst: &Cst, def: usize) -> &str {
        cst.text(def).lines().next().unwrap()
    }

    #[test]
    fn mangled_sig() {
        assert_eq!(
            MangledSig::parse("__ct__9CSandwormFiif"),
            Some(MangledSig { class: Some("CSandworm".into()), params: Some(3) })
        );
        assert_eq!(
            MangledSig::parse("__dt__Q29CSandworm11SEyeEffectsFv"),
            Some(MangledSig { class: Some("CSandworm::SEyeEffects".into()), params: Some(0) })
        );
        assert_eq!(
            MangledSig::parse("GetShakeAngle__9CSandwormCFfff"),
            Some(MangledSig { class: Some("CSandworm".into()), params: Some(3) })
        );
        assert_eq!(
            MangledSig::parse("LoadSandworm__FR13CStateManagerR12CInputStreamR11CEntityInfo"),
            Some(MangledSig { class: None, params: Some(3) })
        );
        // template names carry their arguments inside the length; function pointers and arrays
        let s = "__ct__9CSandwormF9TUniqueIdRCQ24rstl66basic_string<c,Q24rstl14char_traits<c>,Q24rstl17rmemory_allocator>PFi_vA3_fM6CClassFPCvPvi_v";
        assert_eq!(MangledSig::parse(s).unwrap().params, Some(5));
    }

    #[test]
    fn picks_class_and_overload() {
        let cst = Cst::parse(SRC);
        let t = |sym: &str| line_of(&cst, find_target(&cst, sym).unwrap()).to_string();
        assert!(t("__ct__9CSandwormFiif").starts_with("CSandworm::CSandworm(int a, int b, float c)"));
        assert!(t("__ct__9CSandwormFii").starts_with("CSandworm::CSandworm(int a, int b)"));
        assert!(t("__dt__9CSandwormFv").starts_with("CSandworm::~CSandworm"));
        assert!(t("__ct__12CSandwormEyeFi").starts_with("CSandwormEye::CSandwormEye"));
        assert!(t("__ct__Q29CSandworm11SPincerDataFiif").starts_with("inline CSandworm::SPincerData::SPincerData"));
        assert!(t("Initialize__9CSandwormFv").starts_with("void CSandworm::Initialize"));
        assert!(scope_matches("rstl::vector< rstl::pair< int, rstl::string > >", "rstl::vector<Q24rstl4pair<i,Q24rstl6string>>"));
        assert!(!scope_matches("CSandwormEye", "CSandworm"));
        // no name match: the last definition
        assert!(t("Missing__9CSandwormFv").starts_with("CSandworm::SEyeEffects::~SEyeEffects"));
    }
}
