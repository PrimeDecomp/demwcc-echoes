//! After a search: which of its edits carry the gain. The search's best differs from its starting point by many
//! edits, most of them neutral (`16.f` -> `16.`, operand swaps, `__typeof__` temporaries); [`ablate`] reverts each
//! that the fitness does not need, first line by line, then word by word, and leaves the few that matter.
use mwdec_search::score::{Eval, Fitness};
use mwdec_search::Scorer;
use serde::Serialize;
use similar::{Algorithm, DiffTag};
use std::time::Instant;

/// One edit of the base: the bytes `from..to` of it replaced by `text`.
#[derive(Clone, Debug)]
struct Edit {
    from: usize,
    to: usize,
    text: String,
}

/// An edit the result keeps: its line in the base, and the text it replaces and its replacement.
#[derive(Clone, Debug, Serialize)]
pub struct Kept {
    pub line: usize,
    pub before: String,
    pub after: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Ablation {
    /// The edits from the base to the search's best (line hunks).
    pub edits: usize,
    /// The edits left, word by word.
    pub kept: Vec<Kept>,
    pub evals: usize,
    /// Stopped at its deadline (some edits not tried).
    pub cut_short: bool,
    #[serde(skip)]
    pub src: String,
}

/// TEXT's lines, or its tokens: words (identifiers and numbers, `16.f`), runs of white space, and each other
/// character.
fn tokens(text: &str, words: bool) -> Vec<&str> {
    if !words {
        return text.split_inclusive('\n').collect();
    }
    let class = |c: char| if c.is_alphanumeric() || c == '_' || c == '.' { 1 } else if c.is_whitespace() { 2 } else { 0 };
    let mut out = vec![];
    let mut start = 0;
    let mut previous = None;
    for (i, c) in text.char_indices() {
        let k = class(c);
        if i > start && (k == 0 || Some(k) != previous) {
            out.push(&text[start..i]);
            start = i;
        }
        previous = Some(k);
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// The edits from `base` to `best`: the changed runs of a line (`words` false) or token diff.
fn edits(base: &str, best: &str, words: bool) -> Vec<Edit> {
    let (old, new) = (tokens(base, words), tokens(best, words));
    let ops = similar::capture_diff_slices(Algorithm::Myers, &old, &new);
    let offsets = |slices: &[&str]| -> Vec<usize> {
        let mut at = vec![0];
        for s in slices {
            at.push(at.last().unwrap() + s.len());
        }
        at
    };
    let (old_at, new_at) = (offsets(&old), offsets(&new));
    let mut out: Vec<Edit> = vec![];
    let mut open: Option<(usize, usize, usize, usize)> = None;
    for op in &ops {
        let (tag, o, n) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            if let Some((o0, o1, n0, n1)) = open.take() {
                out.push(Edit { from: old_at[o0], to: old_at[o1], text: best[new_at[n0]..new_at[n1]].to_string() });
            }
            continue;
        }
        open = Some(match open {
            Some((o0, _, n0, _)) => (o0, o.end, n0, n.end),
            None => (o.start, o.end, n.start, n.end),
        });
    }
    if let Some((o0, o1, n0, n1)) = open {
        out.push(Edit { from: old_at[o0], to: old_at[o1], text: best[new_at[n0]..new_at[n1]].to_string() });
    }
    out
}

/// `base` with the edits `edits` whose flag in `on` is set.
fn apply(base: &str, edits: &[Edit], on: &[bool]) -> String {
    let mut out = String::with_capacity(base.len());
    let mut at = 0;
    for (e, _) in edits.iter().zip(on).filter(|(_, on)| **on) {
        out.push_str(&base[at..e.from]);
        out.push_str(&e.text);
        at = e.to;
    }
    out.push_str(&base[at..]);
    out
}

fn fitness(scorer: &Scorer, src: &str) -> Option<Fitness> {
    match scorer.eval(src).0 {
        Eval::Ok(f) => Some(f),
        _ => None,
    }
}

/// Revert, one at a time, each of the edits from `base` that `src` needs no more than `goal` is reached: the
/// source with the fewest edits whose fitness is `goal`'s. Edits are tried in order and a reverted one stays
/// reverted (greedy, one compile each).
fn reduce(scorer: &Scorer, base: &str, src: &str, goal: &Fitness, words: bool, deadline: Instant, evals: &mut usize) -> (String, bool) {
    let all = edits(base, src, words);
    let mut on = vec![true; all.len()];
    for i in 0..all.len() {
        if Instant::now() >= deadline {
            return (apply(base, &all, &on), true);
        }
        on[i] = false;
        *evals += 1;
        let keep_reverted = fitness(scorer, &apply(base, &all, &on)).is_some_and(|f| !goal.better_than(&f));
        on[i] = !keep_reverted;
    }
    (apply(base, &all, &on), false)
}

/// The fewest of the edits from `base` to `best` that keep `best`'s fitness (`goal`), by line hunks and then by
/// words, until `deadline`. The result is confirmed with a normal compile; one that does not reach `goal` there gives
/// `best` back.
pub fn ablate(scorer: &Scorer, base: &str, best: &str, goal: &Fitness, deadline: Instant) -> Ablation {
    let mut evals = 0;
    let line_edits = edits(base, best, false).len();
    let (mut src, mut cut_short) = reduce(scorer, base, best, goal, false, deadline, &mut evals);
    if !cut_short && edits(base, &src, true).len() <= 400 {
        (src, cut_short) = reduce(scorer, base, &src, goal, true, deadline, &mut evals);
    }
    evals += 1;
    if src != best && !matches!(scorer.eval_normal(&src).0, Eval::Ok(f) if !goal.better_than(&f)) {
        src = best.to_string();
    }
    let kept = edits(base, &src, true)
        .into_iter()
        .map(|e| Kept { line: base[..e.from].matches('\n').count() + 1, before: base[e.from..e.to].to_string(), after: e.text })
        .collect();
    Ablation { edits: line_edits, kept, evals, cut_short, src }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_rebuild_their_target() {
        let base = "int f(float x) {\n    return x * 16.f;\n}\nint g;\n";
        let best = "int f(float x) {\n    float t = x;\n    return t * 16.;\n}\nint g;\n";
        for words in [false, true] {
            let e = edits(base, best, words);
            assert!(!e.is_empty());
            assert_eq!(apply(base, &e, &vec![true; e.len()]), best);
            assert_eq!(apply(base, &e, &vec![false; e.len()]), base);
        }
        // word by word, `16.f` -> `16.` is an edit of its own
        let e = edits(base, best, true);
        assert!(e.iter().any(|e| base[e.from..e.to].contains("16.f") && !e.text.contains("16.f")), "{e:?}");
    }
}
