//! Load MWCC/dtk ELF objects into `mwdec_core::ObjectFile` (mwcc-compare's loader), and
//! disassemble.
use anyhow::{Context, Result};
use mwdec_core::*;

/// Short display name of a reloc kind (as in objdump / asm syntax).
pub fn reloc_kind_name(k: RelocKind) -> String {
    match k {
        RelocKind::Addr32 => "ADDR32".into(),
        RelocKind::Addr16Lo => "@l".into(),
        RelocKind::Addr16Hi => "@h".into(),
        RelocKind::Addr16Ha => "@ha".into(),
        RelocKind::Rel24 => "REL24".into(),
        RelocKind::Rel14 => "REL14".into(),
        RelocKind::EmbSda21 => "@sda21".into(),
        RelocKind::Other(n) => format!("R_PPC_{n}"),
    }
}

/// Load every function (STT_FUNC in executable sections, any binding) and data symbol with relocations.
pub fn load_object(path: &str) -> Result<ObjectFile> {
    let data = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    load_object_bytes(path, &data)
}

/// Like [`load_object`] but from bytes; `path` only names the object in errors.
pub fn load_object_bytes(path: &str, data: &[u8]) -> Result<ObjectFile> {
    mwcc_compare::strict::elf::load(data).with_context(|| format!("parsing ELF {path}"))
}

/// Find a function by mangled name.
pub fn find_function<'a>(obj: &'a ObjectFile, name: &str) -> Option<&'a Function> {
    obj.functions.iter().find(|f| f.name == name)
}

/// Read up to `len` bytes of a data symbol starting at `addend` (may run past the symbol's
/// declared size into the rest of its section, e.g. for string pools). `None` if the symbol is
/// not defined in this object or lies in a bss-like section.
pub fn data_bytes<'a>(obj: &'a ObjectFile, symbol: &str, addend: i64, len: usize) -> Option<&'a [u8]> {
    let (sec, base) = symbol_location(obj, symbol)?;
    let start = base as i64 + addend;
    if start < 0 || start as usize > sec.bytes.len() {
        return None;
    }
    let s = start as usize;
    Some(&sec.bytes[s..(s + len).min(sec.bytes.len())])
}

/// Section and section-relative address of a defined data symbol (or of a section name itself,
/// which is what unresolvable section-relative relocations target).
pub fn symbol_location<'a>(obj: &'a ObjectFile, symbol: &str) -> Option<(&'a Section, u32)> {
    if let Some(d) = obj.symbols.iter().find(|s| s.name == symbol) {
        let sec = obj.sections.iter().find(|s| s.name == d.section)?;
        return Some((sec, d.address));
    }
    obj.sections.iter().find(|s| s.name == symbol).map(|s| (s, 0))
}

/// The NUL-terminated byte string at `symbol + addend` (without the NUL), if defined here.
pub fn c_string_at<'a>(obj: &'a ObjectFile, symbol: &str, addend: i64) -> Option<&'a [u8]> {
    let b = data_bytes(obj, symbol, addend, usize::MAX / 2)?;
    let end = b.iter().position(|&c| c == 0)?;
    Some(&b[..end])
}

fn fmt_reloc(r: &Reloc) -> String {
    let add = if r.addend == 0 {
        String::new()
    } else if r.addend > 0 {
        format!("+0x{:x}", r.addend)
    } else {
        format!("-0x{:x}", -r.addend)
    };
    format!("{} {}{}", reloc_kind_name(r.kind), r.target, add)
}

/// Disassemble a single instruction word at function offset `off` (no reloc info).
pub fn disasm_word(word: u32, off: u32) -> String {
    let ins = ppc750cl::Ins::new(word);
    let parsed = ins.simplified();
    let mut text = parsed.to_string();
    if ins.is_direct_branch() {
        if let Some(dest) = ins.branch_dest(off) {
            // Replace the relative displacement with a function-relative target.
            if let Some(pos) = text.rfind([' ', ',']) {
                text.truncate(pos + 1);
                text.push_str(&format!("0x{dest:x}"));
            }
        }
    }
    text
}

/// One line per instruction: "offset: word  mnemonic operands  [reloc]".
pub fn disassemble(f: &Function) -> Vec<String> {
    let mut out = Vec::new();
    let mut ri = 0usize;
    for (i, word) in f.words().enumerate() {
        let off = (i * 4) as u32;
        let mut line = format!("{off:5x}: {word:08x}  {}", disasm_word(word, off));
        while ri < f.relocs.len() && f.relocs[ri].offset < off {
            ri += 1;
        }
        let mut rs = Vec::new();
        let mut rj = ri;
        while rj < f.relocs.len() && f.relocs[rj].offset < off + 4 {
            rs.push(fmt_reloc(&f.relocs[rj]));
            rj += 1;
        }
        if !rs.is_empty() {
            let pad = 44usize.saturating_sub(line.len());
            line.push_str(&" ".repeat(pad));
            line.push_str(&format!("  [{}]", rs.join("; ")));
        }
        out.push(line);
    }
    out
}
