//! Check the in-process tracer (the oracle module) against the debugger tracer: trace each source with both and
//! count, per field of each stream, the values that differ (and how long each tracer took).
//!
//! usage: trace-parity [--coloring] [--sched] [--inline] [--pcode] [--iro] [--iro-all] [--copyprop] [--peephole]
//!                     [--filter F] [-v] FILE...
//! (no stream named: all but --iro-all; FILE: a self-contained source, such as a preprocessed unit)
use mwdec_oracle::compile::Compiler;
use mwdec_oracle::tracer::{trace_source_with, Backend, TraceOptions};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Per field path: (values compared, values that differ, the first differences).
type Tally = BTreeMap<String, (u64, u64, Vec<String>)>;

fn diff(a: &Value, b: &Value, path: &str, where_: &str, tally: &mut Tally) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in x {
                diff(v, y.get(k).unwrap_or(&Value::Null), &format!("{path}.{k}"), where_, tally);
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            leaf(&Value::from(x.len()), &Value::from(y.len()), &format!("{path}.len"), where_, tally);
            for (i, (u, v)) in x.iter().zip(y).enumerate() {
                diff(u, v, &format!("{path}[]"), &format!("{where_}[{i}]"), tally);
            }
        }
        _ => leaf(a, b, path, where_, tally),
    }
}

fn leaf(a: &Value, b: &Value, path: &str, where_: &str, tally: &mut Tally) {
    let e = tally.entry(path.to_string()).or_default();
    e.0 += 1;
    if a != b {
        e.1 += 1;
        if e.2.len() < 5 {
            let short = |v: &Value| v.to_string().chars().take(160).collect::<String>();
            e.2.push(format!("{where_}: debugger {} / oracle {}", short(a), short(b)));
        }
    }
}

fn main() -> anyhow::Result<()> {
    mwdec_core::memcap::install();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut files = vec![];
    let mut streams = vec![];
    let mut filter = None;
    let mut verbose = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--coloring" | "--sched" | "--inline" | "--pcode" | "--iro" | "--iro-all" | "--copyprop" | "--peephole" => {
                streams.push(args[i][2..].to_string())
            }
            "--filter" => {
                i += 1;
                filter = args.get(i).cloned();
            }
            "-v" => verbose = true,
            f => files.push(f.to_string()),
        }
        i += 1;
    }
    if streams.is_empty() {
        streams = ["coloring", "sched", "inline", "pcode", "iro", "copyprop", "peephole"].map(String::from).to_vec();
    }
    let comp = Compiler::default();
    let mut tally = Tally::new();
    let (mut debugger_time, mut oracle_time) = (Duration::ZERO, Duration::ZERO);
    let mut failures = 0;
    // (the peephole's rules as each tracer names them)
    let mut rules: BTreeMap<(String, String), u64> = BTreeMap::new();
    for file in &files {
        let source = std::fs::read_to_string(file)?;
        for stream in &streams {
            let opts = TraceOptions {
                coloring: stream == "coloring",
                sched: stream == "sched",
                sched_filter: filter.clone().filter(|_| stream == "sched"),
                inline: stream == "inline",
                pcode: if stream == "pcode" {
                    ["initial", "presched", "precolor"].map(String::from).to_vec()
                } else {
                    vec![]
                },
                iro: stream.starts_with("iro"),
                iro_all_stages: stream == "iro-all",
                copyprop: stream == "copyprop",
                peephole: stream == "peephole",
            };
            let t0 = Instant::now();
            let debugger = trace_source_with(&comp, &source, &opts, Backend::Debugger);
            let t1 = Instant::now();
            let oracle = trace_source_with(&comp, &source, &opts, Backend::Oracle);
            let t2 = Instant::now();
            debugger_time += t1 - t0;
            oracle_time += t2 - t1;
            let (debugger, oracle) = match (debugger, oracle) {
                (Ok(d), Ok(o)) => (d, o),
                (d, o) => {
                    failures += 1;
                    eprintln!("{file} {stream}: debugger {:?} / oracle {:?}", d.err(), o.err());
                    continue;
                }
            };
            let where_ = format!("{file} {stream}");
            // (each trace compiles a source file of its own name, which names its static initializer; a branch's
            // label operand is printed as its address)
            let stem = regex::bytes::Regex::new(r"tr\d+_\d+").unwrap();
            let same = stem.replace_all(&debugger.object, &b"tr"[..]) == stem.replace_all(&oracle.object, &b"tr"[..]);
            leaf(&Value::from(same), &Value::from(true), "object", &where_, &mut tally);
            let normal = |t: &mwdec_oracle::tracer::Trace| -> anyhow::Result<Value> {
                let text = serde_json::to_string(t)?;
                let text = regex::Regex::new(r"tr\d+_\d+").unwrap().replace_all(&text, "tr");
                let text = regex::Regex::new(r"<4:\d+>").unwrap().replace_all(&text, "<4:label>");
                // (the dump file is written in text mode by the executable)
                let text = text.replace(r"\r\n", r"\n");
                // (an optimizer flowgraph node's mustreach1 is set only by the loop analysis: memory before that)
                let text = regex::Regex::new(r"MustReach1 ?= ?\d+").unwrap().replace_all(&text, "MustReach1=?");
                Ok(serde_json::from_str(&text)?)
            };
            let (d, o) = (normal(&debugger)?, normal(&oracle)?);
            // (TRACE_PARITY_DUMP=DIR: both traces, as compared, for a closer look)
            if let Some(dir) = std::env::var_os("TRACE_PARITY_DUMP") {
                let stem = std::path::Path::new(file).file_stem().unwrap_or_default().to_string_lossy().into_owned();
                for (side, v) in [("debugger", &d), ("oracle", &o)] {
                    let path = std::path::Path::new(&dir).join(format!("{stem}.{stream}.{side}.json"));
                    std::fs::write(path, serde_json::to_string_pretty(v)?)?;
                }
            }
            diff(&d, &o, "", &where_, &mut tally);
            for (a, b) in debugger.peephole.iter().zip(&oracle.peephole) {
                *rules.entry((a.rule.clone(), b.rule.clone())).or_insert(0) += 1;
            }
            if verbose {
                eprintln!("{where_}: {:?} / {:?}", t1 - t0, t2 - t1);
            }
        }
    }
    let mut differing = 0;
    for (path, (compared, differ, examples)) in &tally {
        if *differ > 0 {
            differing += 1;
            println!("{path}: {differ} of {compared} differ");
            for e in examples {
                println!("    {e}");
            }
        } else if verbose {
            println!("{path}: {compared} equal");
        }
    }
    if verbose {
        for ((a, b), n) in &rules {
            println!("rule {a} = {b} ({n})");
        }
    }
    let compared: u64 = tally.values().map(|t| t.0).sum();
    println!(
        "{} files, {compared} values compared, {differing} fields differ, {failures} failures; debugger {:.1} s, oracle {:.1} s",
        files.len(),
        debugger_time.as_secs_f64(),
        oracle_time.as_secs_f64()
    );
    Ok(())
}
