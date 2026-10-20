//! Operator tests: each operator applied with many seeds to a small function; the distinct
//! results (normalized) must contain the expected form (a site), be empty where there is no
//! site, and never contain a form that changes what the code does (semantics guards).
use crate::cst::normalize;
use crate::opctx::OpContext;
use crate::ops::{op_index, Parsed, OPS};
use crate::rng::Rng;

/// Distinct normalized results of operator `op` on `src` (target function `Foo`).
fn outs_ctx(src: &str, op: &str, ctx: Option<&OpContext>) -> Vec<String> {
    let i = op_index(op).unwrap_or_else(|| panic!("no operator {op}"));
    let p = Parsed::new(src, "Foo__Fv").expect("target function").with_ctx(ctx);
    let mut v: Vec<String> = Vec::new();
    for seed in 0..128 {
        if let Some(s) = p.apply(i, &mut Rng::new(seed), None) {
            let n = normalize(&s);
            if !v.contains(&n) {
                v.push(n);
            }
        }
    }
    v
}

fn outs(src: &str, op: &str) -> Vec<String> {
    outs_ctx(src, op, None)
}

/// Some result contains `frag` (normalized).
fn has(v: &[String], frag: &str) -> bool {
    let f = normalize(frag);
    v.iter().any(|o| o.contains(&f))
}

#[track_caller]
fn expect(v: &[String], frag: &str) {
    assert!(has(v, frag), "expected `{frag}` in:\n{}", v.join("\n"));
}

#[track_caller]
fn never(v: &[String], frag: &str) {
    assert!(!has(v, frag), "unexpected `{frag}` in:\n{}", v.join("\n"));
}

#[test]
fn operator_names_are_unique() {
    for (i, o) in OPS.iter().enumerate() {
        assert_eq!(op_index(o.name), Some(i), "duplicate operator name {}", o.name);
    }
}

// ------------------------------------------------------------------ call_ternary

#[test]
fn call_ternary_splits_and_merges() {
    let v = outs("void Foo(int c) { SetText(x, c ? 1 : 2); }", "call_ternary");
    expect(&v, "if (c) { SetText(x, 1); } else { SetText(x, 2); }");
    let v = outs("void Foo(int c) { if (c) { g(1, a); } else { g(2, a); } }", "call_ternary");
    expect(&v, "g(c ? 1 : 2, a);");
}

#[test]
fn call_ternary_no_site() {
    assert!(outs("int Foo(int c) { return c ? 1 : 2; }", "call_ternary").is_empty());
    // the arms call different functions
    assert!(outs("void Foo(int c) { if (c) { g(1); } else { h(2); } }", "call_ternary").is_empty());
}

#[test]
fn call_ternary_keeps_evaluation_order() {
    // the condition and the other argument both have side effects: no reordering
    assert!(outs("void Foo() { SetText(Next(), Pick() ? 1 : 2); }", "call_ternary").is_empty());
    // the arms differ in two arguments
    assert!(outs("void Foo(int c) { if (c) { g(1, a); } else { g(2, b); } }", "call_ternary").is_empty());
}

// ------------------------------------------------------------------ tail_dup

#[test]
fn tail_dup_duplicates_and_joins() {
    let v = outs("void Foo(int a, int b) { if (a && b) { x = 1; } else { x = 2; } }", "tail_dup");
    expect(&v, "if (a) { if (b) { x = 1; } else { x = 2; } } else { x = 2; }");
    let v = outs("void Foo(int a, int b) { if (a || b) { x = 1; } else { x = 2; } }", "tail_dup");
    expect(&v, "if (a) { x = 1; } else { if (b) { x = 1; } else { x = 2; } }");
    let v = outs("void Foo(int a, int b) { if (a) { if (b) { x = 1; } else { x = 2; } } else { x = 2; } }", "tail_dup");
    expect(&v, "if (a && b) { x = 1; } else { x = 2; }");
    // `else if (c) S else S` -> `else S`
    let v = outs("void Foo(int a, int b) { if (a) { x = 1; } else if (b) { x = 2; } else { x = 2; } }", "tail_dup");
    expect(&v, "if (a) { x = 1; } else { x = 2; }");
}

#[test]
fn tail_dup_no_site() {
    assert!(outs("void Foo(int a, int b) { if (a && b) { x = 1; } }", "tail_dup").is_empty());
}

#[test]
fn tail_dup_guards() {
    // a test with side effects is not dropped even when both arms are the same
    let v = outs("void Foo(int a) { if (a) { x = 1; } else if (Check()) { x = 2; } else { x = 2; } }", "tail_dup");
    never(&v, "if (a) { x = 1; } else { x = 2; }");
    // a labelled statement is never duplicated
    let v = outs("void Foo(int a, int b) { if (a && b) { x = 1; } else { L: x = 2; } }", "tail_dup");
    never(&v, "if (a) {");
}

// ------------------------------------------------------------------ bool_compare

#[test]
fn bool_compare_forms() {
    let v = outs("void Foo(bool b) { if (b) { g(); } }", "bool_compare");
    expect(&v, "if (b == true)");
    let v = outs("void Foo(bool b) { if (!b) { g(); } }", "bool_compare");
    assert!(has(&v, "if (b == false)") || has(&v, "if (b != true)"), "{v:?}");
    let v = outs("void Foo(bool b) { if (b == true) { g(); } if (b != true) { h(); } }", "bool_compare");
    expect(&v, "if (b) { g(); }");
    expect(&v, "if (!b) { h(); }");
    // a member declared bool in the context
    let ctx = OpContext { bool_members: ["valid".to_string()].into(), ..Default::default() };
    let v = outs_ctx("void Foo(CThing* t) { if (t->valid()) { g(); } }", "bool_compare", Some(&ctx));
    expect(&v, "if (t->valid() == true)");
}

#[test]
fn bool_compare_no_site() {
    // not known to be bool
    assert!(outs("void Foo(int i) { if (i) { g(); } }", "bool_compare").is_empty());
    assert!(outs("void Foo(CThing* t) { if (t->valid()) { g(); } }", "bool_compare").is_empty());
}

#[test]
fn bool_compare_keeps_negation() {
    let v = outs("void Foo(bool a, bool b) { if (a && !b) { g(); } }", "bool_compare");
    // `!b` -> `b == false`, never `b == true`
    never(&v, "a && b == true");
    never(&v, "a && (b == true)");
}

// ------------------------------------------------------------------ case_insert

fn state_enum() -> OpContext {
    OpContext { enums: vec![("CFoo::EState".into(), vec![("kA".into(), 0), ("kB".into(), 1), ("kC".into(), 2), ("kAlias".into(), 0)])], ..Default::default() }
}

#[test]
fn case_insert_adds_and_removes() {
    let ctx = state_enum();
    let v = outs_ctx("void Foo(int s) { switch (s) { case kA: g(); break; case kB: h(); break; } }", "case_insert", Some(&ctx));
    expect(&v, "case kC: break;");
    // qualified labels keep their qualifier
    let v = outs_ctx("void Foo(int s) { switch (s) { case CFoo::kA: g(); break; } }", "case_insert", Some(&ctx));
    expect(&v, "case CFoo::kC: break;");
    let v = outs_ctx("void Foo(int s) { switch (s) { case kA: g(); break; case kC: break; } }", "case_insert", Some(&ctx));
    expect(&v, "{ case kA: g(); break; } }");
}

#[test]
fn case_insert_no_site() {
    let ctx = state_enum();
    let src = "void Foo(int s) { switch (s) { case kA: g(); break; case kB: h(); break; } }";
    // no context, or a default (an added case would leave it)
    assert!(outs(src, "case_insert").is_empty());
    assert!(outs_ctx("void Foo(int s) { switch (s) { case kA: g(); break; default: h(); break; } }", "case_insert", Some(&ctx)).is_empty());
    // labels of no known enum
    assert!(outs_ctx("void Foo(int s) { switch (s) { case 1: g(); break; } }", "case_insert", Some(&ctx)).is_empty());
}

#[test]
fn case_insert_guards() {
    let ctx = state_enum();
    // an enumerator whose value is present already is never added (duplicate case value)
    let v = outs_ctx("void Foo(int s) { switch (s) { case kA: g(); break; case kB: h(); break; } }", "case_insert", Some(&ctx));
    never(&v, "case kAlias");
    // a case that the previous one falls into is not removed, nor is a case put between them
    let v = outs_ctx("void Foo(int s) { switch (s) { case kA: g(); case kB: break; } }", "case_insert", Some(&ctx));
    never(&v, "{ case kA: g(); } }");
    never(&v, "g(); case kC");
}

// ------------------------------------------------------------------ case_return

#[test]
fn case_return_forms() {
    let v = outs("void Foo(int s) { switch (s) { case 1: g(); break; case 2: h(); break; } }", "case_return");
    expect(&v, "case 1: g(); return;");
    expect(&v, "case 1: g(); return; case 2: h(); return;");
    let v = outs("void Foo(int s) { switch (s) { case 1: g(); return; } }", "case_return");
    expect(&v, "case 1: g(); break;");
}

#[test]
fn case_return_no_site() {
    // something follows the switch, or the function returns a value
    assert!(outs("void Foo(int s) { switch (s) { case 1: g(); break; } h(); }", "case_return").is_empty());
    assert!(outs("int Foo(int s) { switch (s) { case 1: g(); break; } return 0; }", "case_return").is_empty());
}

#[test]
fn case_return_leaves_inner_loops() {
    let v = outs("void Foo(int s) { switch (s) { case 1: for (;;) { if (g()) break; } break; } }", "case_return");
    expect(&v, "} return; } }");
    never(&v, "if (g()) return;");
    let v = outs("void Foo(int s) { switch (s) { case 1: while (g()) { return; } break; } }", "case_return");
    never(&v, "{ break; }");
}

// ------------------------------------------------------------------ invert_early

#[test]
fn invert_early_both_ways() {
    let v = outs("void Foo(int a) { if (a) { g(); return; } h(); k(); }", "invert_early");
    expect(&v, "if (!a) { h(); k(); } else { g(); }");
    let v = outs("void Foo(int a) { if (a) { h(); } else { g(); } }", "invert_early");
    expect(&v, "if (!a) { g(); return; } h();");
    // with a value: the returns stay
    let v = outs("int Foo(int a) { if (a == 1) { g(); return 1; } h(); return 2; }", "invert_early");
    expect(&v, "if (a != 1) { h(); return 2; } else { g(); return 1; }");
}

#[test]
fn invert_early_no_site() {
    assert!(outs("void Foo(int a) { if (a) { g(); } h(); }", "invert_early").is_empty());
}

#[test]
fn invert_early_needs_the_function_end() {
    // code after the enclosing block: dropping the return would run it
    let v = outs("void Foo(int a) { { if (a) { g(); return; } h(); } k(); }", "invert_early");
    never(&v, "if (!a) { h(); }");
    assert!(v.is_empty(), "{v:?}");
}

// ------------------------------------------------------------------ incr_test

#[test]
fn incr_test_folds_and_splits() {
    let v = outs("void Foo(int n) { ++n; if (n == 3) { g(); } }", "incr_test");
    expect(&v, "if (++n == 3) { g(); }");
    let v = outs("void Foo(CObj* p) { p->mCount -= 1; if (0 == p->mCount) { g(); } }", "incr_test");
    expect(&v, "if (0 == --p->mCount)");
    let v = outs("void Foo(int n) { if (--n == 0) { g(); } }", "incr_test");
    expect(&v, "--n; if (n == 0) { g(); }");
}

#[test]
fn incr_test_no_site() {
    assert!(outs("void Foo(int n, int m) { ++n; if (m == 3) { g(); } }", "incr_test").is_empty());
}

#[test]
fn incr_test_guards() {
    // the other operand reads the variable too, or the test is a postfix increment
    assert!(outs("void Foo(int n) { ++n; if (n == n * 2) { g(); } }", "incr_test").is_empty());
    assert!(outs("void Foo(int n) { if (n++ == 3) { g(); } }", "incr_test").is_empty());
}

// ------------------------------------------------------------------ float_literal (extended)

#[test]
fn float_literal_reciprocal_forms() {
    let v = outs("float Foo(float e) { return e * 0.5f; }", "float_literal");
    expect(&v, "e / 2.0f");
    let v = outs("float Foo(float e) { return 0.5f * e; }", "float_literal");
    expect(&v, "e / 2.0f");
    let v = outs("void Foo(float e) { e *= 0.5f; }", "float_literal");
    expect(&v, "e /= 2.0f");
    // not a power of two: no exact reciprocal
    never(&outs("float Foo(float e) { return e * 0.3f; }", "float_literal"), "e /");
}

// ------------------------------------------------------------------ static_literal

#[test]
fn static_literal_both_ways() {
    let v = outs("float Foo(float x) { return x * 1.5f; }", "static_literal");
    expect(&v, "static const float kLit0 = 1.5f; return x * kLit0;");
    let v = outs("float Foo(float x) { static const float k = 1.5f; return x * k; }", "static_literal");
    expect(&v, "{ return x * 1.5f; }");
}

#[test]
fn static_literal_no_site() {
    assert!(outs("int Foo(int x) { return x * 3; }", "static_literal").is_empty());
}

#[test]
fn static_literal_guards() {
    // a literal initialising a static object stays where it is
    let v = outs("float Foo(float x) { static float s = 2.0f; return x + s; }", "static_literal");
    never(&v, "s = kLit");
    // a static constant that is written is not inlined (not `static const`)
    never(&outs("float Foo(float x) { static float s = 2.0f; s = x; return s; }", "static_literal"), "return 2.0f;");
}

// ------------------------------------------------------------------ vector forms

fn dims() -> OpContext {
    OpContext { dims: Some(["kDX".into(), "kDY".into(), "kDZ".into()]), ..Default::default() }
}

#[test]
fn vec_index_forms() {
    let ctx = dims();
    let v = outs_ctx("void Foo(CVector3f& v, float h) { v.SetZ(v.GetZ() + h); }", "vec_index", Some(&ctx));
    expect(&v, "v[kDZ] += h;");
    expect(&v, "v.SetZ(v[kDZ] + h);");
    let v = outs_ctx("void Foo(CVector3f& v, float h) { v.SetX(h); }", "vec_index", Some(&ctx));
    expect(&v, "v[kDX] = h;");
    let v = outs_ctx("float Foo(const CVector3f& v) { return v.GetY(); }", "vec_index", Some(&ctx));
    expect(&v, "return v[kDY];");
    let v = outs_ctx("void Foo(CVector3f& v, float h) { v[kDZ] += h; }", "vec_index", Some(&ctx));
    expect(&v, "v.SetZ(v.GetZ() + h);");
    let v = outs_ctx("float Foo(const CVector3f& v) { return v[kDX]; }", "vec_index", Some(&ctx));
    expect(&v, "return v.GetX();");
}

#[test]
fn vec_index_no_site() {
    // without the context's index constants
    assert!(outs("float Foo(const CVector3f& v) { return v.GetY(); }", "vec_index").is_empty());
    assert!(outs_ctx("float Foo(const float* v) { return v[1]; }", "vec_index", Some(&dims())).is_empty());
}

#[test]
fn vec_index_guards() {
    let v = outs_ctx("void Foo(CVector3f& v) { v[kDX]++; }", "vec_index", Some(&dims()));
    never(&v, "GetX()++");
}

#[test]
fn vec_dot_both_ways() {
    let v = outs("float Foo(const CVector3f& v) { return v.MagSquared(); }", "vec_dot");
    expect(&v, "return CVector3f::Dot(v, v);");
    let v = outs("float Foo(const CVector3f& v) { return CVector3f::Dot(v, v); }", "vec_dot");
    expect(&v, "return v.MagSquared();");
}

#[test]
fn vec_dot_no_site() {
    // a member's type is not known
    assert!(outs("float Foo(CThing* t) { return t->mPos.MagSquared(); }", "vec_dot").is_empty());
}

#[test]
fn vec_dot_guards() {
    assert!(outs("float Foo(const CVector3f& a, const CVector3f& b) { return CVector3f::Dot(a, b); }", "vec_dot").is_empty());
    assert!(outs("float Foo() { return CVector3f::Dot(Next(), Next()); }", "vec_dot").is_empty());
}

#[test]
fn vec_copy_both_ways() {
    let v = outs("void Foo(const CVector3f& a, float z) { CVector3f p(a.GetX(), a.GetY(), z); Use(p); }", "vec_copy");
    expect(&v, "CVector3f p = a; p.SetZ(z); Use(p);");
    let v = outs_ctx("void Foo(const CVector3f& a, float z) { CVector3f p(a.GetX(), a.GetY(), z); Use(p); }", "vec_copy", Some(&dims()));
    expect(&v, "CVector3f p = a; p[kDZ] = z;");
    let v = outs("void Foo(const CVector3f& a, float y) { CVector3f p = a; p.SetY(y); Use(p); }", "vec_copy");
    expect(&v, "CVector3f p(a.GetX(), y, a.GetZ()); Use(p);");
}

#[test]
fn vec_copy_no_site() {
    assert!(outs("void Foo(const CVector3f& a, const CVector3f& b, float z) { CVector3f p(a.GetX(), b.GetY(), z); Use(p); }", "vec_copy").is_empty());
}

#[test]
fn vec_copy_guards() {
    // the stored value reads the object itself
    assert!(outs("void Foo(const CVector3f& a) { CVector3f p = a; p.SetZ(p.GetX()); Use(p); }", "vec_copy").is_empty());
}

#[test]
fn float_view_both_ways() {
    let v = outs("void Foo() { CVector3f v = Get(); Draw(v.GetX(), v.GetY(), v.GetZ()); }", "float_view");
    expect(&v, "CVector3f v = Get(); const float* fv0 = (const float*)&v; Draw(fv0[0], fv0[1], fv0[2]);");
    let v = outs("void Foo() { CVector3f v = Get(); const float* f = (const float*)&v; Draw(f[0], f[1], f[2]); }", "float_view");
    expect(&v, "CVector3f v = Get(); Draw(v.GetX(), v.GetY(), v.GetZ());");
}

#[test]
fn float_view_no_site() {
    // components not passed to a call
    assert!(outs("float Foo() { CVector3f v = Get(); return v.GetX(); }", "float_view").is_empty());
}

#[test]
fn float_view_guards() {
    // the context knows the class is not three floats
    let ctx = OpContext { vec3: [("CVector3f".to_string(), false)].into(), ..Default::default() };
    assert!(outs_ctx("void Foo() { CVector3f v = Get(); Draw(v.GetX(), v.GetY(), v.GetZ()); }", "float_view", Some(&ctx)).is_empty());
    // a view that is written through stays
    assert!(outs("void Foo() { CVector3f v = Get(); float* f = (float*)&v; f[0] = 1.0f; Draw(f[0]); }", "float_view").is_empty());
}

// ------------------------------------------------------------------ loosened operators

#[test]
fn param_const_on_class_by_value() {
    let v = outs("void Foo(TUniqueId id) { g(id); }", "param_const");
    expect(&v, "void Foo(const TUniqueId id)");
    let v = outs("void Foo(const TUniqueId id) { g(id); }", "param_const");
    expect(&v, "void Foo(TUniqueId id)");
}

#[test]
fn param_const_no_site_on_references() {
    assert!(outs("void Foo(const CVector3f& v) { g(v); }", "param_const").is_empty());
}

#[test]
fn inline_var_all_reads_getters_again() {
    let v = outs("void Foo(CActor* a) { float x = a->GetX(); float y = x + 1.0f; g(x, y); }", "inline_var_all");
    expect(&v, "{ float y = a->GetX() + 1.0f; g(a->GetX(), y); }");
    // (getter chains too)
    let v = outs("void Foo(CActor* a) { float x = a->GetPos().GetX(); float y = x + 1.0f; g(x, y); }", "inline_var_all");
    expect(&v, "g(a->GetPos().GetX(), y);");
}

#[test]
fn inline_var_all_getter_guards() {
    // a call or store between the definition and the last use may change what the getter reads
    assert!(outs("void Foo(CActor* a) { float x = a->GetX(); g(x); h(x); }", "inline_var_all").is_empty());
    assert!(outs("void Foo(CActor* a) { float x = a->GetX(); a->Move(); g(x); h(x); }", "inline_var_all").is_empty());
    assert!(outs("void Foo(CActor* a) { float x = a->GetX(); a->mX = 0.0f; g(x); h(x); }", "inline_var_all").is_empty());
    // a getter with arguments is a call
    assert!(outs("void Foo(CActor* a) { float x = a->GetX(1); g(x); h(x); }", "inline_var_all").is_empty());
}

#[test]
fn unwrap_block_case_blocks() {
    let v = outs("void Foo(int s) { switch (s) { case 1: { g(); break; } case 2: break; } }", "unwrap_block");
    expect(&v, "case 1: g(); break; case 2:");
    // the last case may declare
    let v = outs("void Foo(int s) { switch (s) { case 1: break; case 2: { int t = f(); g(t); break; } } }", "unwrap_block");
    expect(&v, "case 2: int t = f(); g(t); break;");
}

#[test]
fn unwrap_block_case_guard() {
    // a later label would jump past the declaration
    assert!(outs("void Foo(int s) { switch (s) { case 1: { int t = f(); g(t); break; } case 2: break; } }", "unwrap_block").is_empty());
}

#[test]
fn wrap_block_case_body() {
    let v = outs("void Foo(int s) { switch (s) { case 1: g(); break; } }", "wrap_block");
    expect(&v, "case 1: { g(); }");
}

#[test]
fn object_scope_both_ways() {
    let v = outs("void Foo() { { CFoo f; f.Run(); } g(); }", "object_scope");
    expect(&v, "{ CFoo f; f.Run(); g(); }");
    let v = outs("void Foo() { CFoo f; f.Run(); g(); }", "object_scope");
    expect(&v, "{ { CFoo f; f.Run(); } g(); }");
}

#[test]
fn object_scope_no_site() {
    assert!(outs("void Foo() { { int i = f(); g(i); } h(); }", "object_scope").is_empty());
}

#[test]
fn object_scope_guards() {
    // the object is used after the run
    let v = outs("void Foo() { CFoo f; f.Run(); g(f); }", "object_scope");
    never(&v, "{ CFoo f; f.Run(); } g(f);");
    // the names of an unwrapped block must not clash
    assert!(outs("void Foo() { { CFoo f; f.Run(); } { CFoo f; f.Run(); } }", "object_scope").iter().all(|o| o.matches("{CFoo f;").count() >= 1));
}

