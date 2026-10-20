//! Facts about the unit's context types that some operators need and the source text alone
//! doesn't give (enumerators of a switch's enum, `bool` members, the index constants of vector
//! `operator[]`s). Built from the drafter's TypeDb (headers only, see DESIGN.md); operators that
//! need it have no site without it.
use mwdec_core::{Type, TypeDb};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpContext {
    /// Enumerations: (qualified name, enumerators with their values).
    pub enums: Vec<(String, Vec<(String, i64)>)>,
    /// Member names (fields, methods) declared `bool` wherever a member of that name is declared.
    pub bool_members: BTreeSet<String>,
    /// Index constants of 3-component vector `operator[]` overloads (X, Y, Z spelling), from
    /// overloads taking a one-enumerator enum whose name ends in X / Y / Z (`kDX`).
    pub dims: Option<[String; 3]>,
    /// Classes by name: true if they are three floats at offsets 0, 4, 8 (12 bytes).
    pub vec3: BTreeMap<String, bool>,
}

/// Last `::` component of a qualified name.
fn last(q: &str) -> &str {
    q.rsplit("::").next().unwrap_or(q)
}

impl OpContext {
    pub fn from_db(db: &TypeDb) -> OpContext {
        let enums: Vec<(String, Vec<(String, i64)>)> = db.enums.iter().filter(|(_, e)| !e.values.is_empty()).map(|(k, e)| (k.clone(), e.values.clone())).collect();
        // member name -> (declared bool somewhere, declared otherwise somewhere)
        let mut seen: BTreeMap<String, (bool, bool)> = BTreeMap::new();
        let mut note = |name: &str, ty: &Type| {
            let e = seen.entry(name.to_string()).or_default();
            if *ty.unqualified() == Type::Bool {
                e.0 = true;
            } else {
                e.1 = true;
            }
        };
        let mut vec3 = BTreeMap::new();
        for (cname, cl) in &db.classes {
            for f in &cl.fields {
                note(&f.name, &f.ty);
            }
            for mth in &cl.methods {
                note(last(&mth.qualified_name), &mth.ret);
            }
            if !cl.is_declaration {
                let floats = |off: u32| cl.fields.iter().any(|f| f.offset == off && matches!(f.ty.unqualified(), Type::Float { size: 4 }));
                vec3.insert(cname.clone(), cl.size == 12 && floats(0) && floats(4) && floats(8));
            }
        }
        let mut dims: [Option<String>; 3] = [None, None, None];
        for (q, ds) in &db.decls {
            if q.contains("::") {
                for d in ds {
                    note(last(q), &d.ret);
                }
            }
            if last(q) != "operator[]" {
                continue;
            }
            for d in ds {
                let [p] = d.params.as_slice() else { continue };
                let Type::Named(en) = p.ty.unqualified() else { continue };
                let Some(e) = db.enums.get(en) else { continue };
                let [(v, _)] = e.values.as_slice() else { continue };
                let Some(k) = ["X", "Y", "Z"].iter().position(|s| last(en).ends_with(s)) else { continue };
                let scope = en.rfind("::").map(|i| &en[..i + 2]).unwrap_or("");
                dims[k].get_or_insert_with(|| format!("{scope}{v}"));
            }
        }
        let bool_members = seen.into_iter().filter(|(_, (b, o))| *b && !*o).map(|(n, _)| n).collect();
        let dims = match dims {
            [Some(x), Some(y), Some(z)] => Some([x, y, z]),
            _ => None,
        };
        OpContext { enums, bool_members, dims, vec3 }
    }

    /// The facts the given sources can use: enums with an enumerator, bool members and classes
    /// whose names occur in them (keeps the draft-server reply small).
    pub fn restrict_to(&self, texts: &[&str]) -> OpContext {
        let words: BTreeSet<&str> = texts.iter().flat_map(|t| t.split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))).filter(|w| !w.is_empty()).collect();
        OpContext {
            enums: self.enums.iter().filter(|(_, vs)| vs.iter().any(|(v, _)| words.contains(v.as_str()))).cloned().collect(),
            bool_members: self.bool_members.iter().filter(|n| words.contains(n.as_str())).cloned().collect(),
            dims: self.dims.clone(),
            vec3: self.vec3.iter().filter(|(n, _)| words.contains(last(n))).map(|(n, b)| (n.clone(), *b)).collect(),
        }
    }
}
