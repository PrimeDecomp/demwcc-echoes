//! Compiler flag profiles: the project's distinct flag sets (its build.ninja's, through
//! mwcc-project), for compiles that have no unit.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// main.dol game code (763 units): GC/2.7, C++, deferred,noauto, inline_max_size(128).
    Game,
    /// REL game code (106 units): Game + `-sdata 0 -sdata2 0 -pool off`.
    Rel,
    /// REL with pooling (3 units): Game + `-sdata 0 -sdata2 0`.
    RelPool,
    /// SDK / runtime C (57 units): GC/2.7, C, deferred,auto, common off.
    SdkC,
    /// Dolphin SDK C (78 units): GC/1.2.5n.
    Sdk125,
    /// MusyX (31 units): GC/1.3.2.
    Musyx,
}

impl Profile {
    pub fn parse(s: &str) -> Option<Profile> {
        Some(match s {
            "game" | "main" => Profile::Game,
            "rel" => Profile::Rel,
            "relpool" | "rel-pool" => Profile::RelPool,
            "sdkc" | "c" => Profile::SdkC,
            "sdk125" | "1.2.5n" => Profile::Sdk125,
            "musyx" | "1.3.2" => Profile::Musyx,
            _ => return None,
        })
    }

    /// Compiler version directory under build/compilers/.
    pub fn version(self) -> &'static str {
        match self {
            Profile::Sdk125 => "GC/1.2.5n",
            Profile::Musyx => "GC/1.3.2",
            _ => "GC/2.7",
        }
    }

    pub fn is_c(self) -> bool {
        matches!(self, Profile::SdkC | Profile::Sdk125 | Profile::Musyx)
    }
}

impl Profile {
    /// The profile of a unit built with `flags` by the compiler `mw_version` (`GC/2.7`).
    pub fn of(flags: &[String], mw_version: &str) -> Profile {
        let has = |a: &str, b: &str| flags.windows(2).any(|w| w[0] == a && w[1] == b);
        match mw_version {
            "GC/1.2.5n" => Profile::Sdk125,
            "GC/1.3.2" => Profile::Musyx,
            _ if flags.iter().any(|f| f == "-lang=c") => Profile::SdkC,
            _ if has("-sdata", "0") && has("-pool", "off") => Profile::Rel,
            _ if has("-sdata", "0") => Profile::RelPool,
            _ => Profile::Game,
        }
    }
}

type Profiles = Vec<(Profile, Vec<String>)>;

/// The flags most of the project's units of each profile build with (mwcc-project's units, in
/// objdiff.json's order: the first set wins a tie), read once per root.
fn project_profiles(root: &Path) -> Profiles {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<std::path::PathBuf, Profiles>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(v) = cache.lock().unwrap().get(root) {
        return v.clone();
    }
    let mut counts: Vec<(Profile, Vec<String>, usize)> = Vec::new();
    if let Ok(p) = mwcc_project::Project::open(root) {
        for name in p.unit_names().unwrap_or_default() {
            let Ok(u) = p.unit(&name) else { continue };
            let profile = Profile::of(&u.flags, &u.mw_version.as_deref().unwrap_or("").replace('\\', "/"));
            match counts.iter_mut().find(|(q, f, _)| *q == profile && *f == u.flags) {
                Some(c) => c.2 += 1,
                None => counts.push((profile, u.flags, 1)),
            }
        }
    }
    let mut out: Profiles = Vec::new();
    for (profile, flags, n) in &counts {
        if out.iter().any(|(q, _)| q == profile) {
            continue;
        }
        let best = counts.iter().filter(|(q, _, _)| q == profile).fold((flags, *n), |b, (_, f, m)| if *m > b.1 { (f, *m) } else { b });
        out.push((*profile, best.0.clone()));
    }
    cache.lock().unwrap().insert(root.to_path_buf(), out.clone());
    out
}

/// Full flag list (without `-c`, input, `-o`) for a profile: the flags most of the project's units
/// of that profile build with ([`Profile::of`]), include paths made absolute against `root`
/// (`root` = project root) so compiles can run from any working directory. Empty if the project
/// has no unit of the profile.
pub fn profile_flags(p: Profile, root: &Path) -> Vec<String> {
    let flags = project_profiles(root).into_iter().find(|(q, _)| *q == p).map(|(_, f)| f).unwrap_or_default();
    let mut out = Vec::with_capacity(flags.len());
    let mut abs_next = false;
    for a in flags {
        if abs_next && !Path::new(&a).is_absolute() {
            out.push(root.join(&a).to_string_lossy().replace('\\', "/"));
        } else {
            out.push(a.clone());
        }
        abs_next = a == "-i" || a == "-I" || a == "-ir";
    }
    out
}
