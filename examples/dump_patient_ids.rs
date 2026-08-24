//! Print the PatientID (0010,0020) carried by every DICOM file under a
//! directory, so a mapper file (r-7) can be checked against the values
//! the images actually hold.
//!
//! Usage:
//!   cargo run --example dump_patient_ids -- <dir> [--counts]
//!
//! Each line is `<count> <TAB> <PatientID>` with `--counts`, or
//! `<path> <TAB> <PatientID>` without it. Values are shown exactly as
//! read, with any non-printable byte escaped, so trailing padding or
//! stray characters are visible rather than invisible.

use dicom_dictionary_std::tags;
use dicom_object::open_file;
use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <dir> [--counts]", args[0]);
        process::exit(1);
    }
    let root = PathBuf::from(&args[1]);
    let counts_only = args.iter().any(|a| a == "--counts");

    let mut files = Vec::new();
    if let Err(e) = collect(&root, &mut files) {
        eprintln!("Error reading {}: {}", root.display(), e);
        process::exit(1);
    }
    files.sort();

    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
    for path in &files {
        let shown = match read_patient_id(path) {
            Ok(Some(value)) => escape(&value),
            Ok(None) => "<absent>".to_string(),
            Err(e) => format!("<unreadable: {}>", e),
        };
        if counts_only {
            *tally.entry(shown).or_default() += 1;
        } else {
            println!("{}\t{}", path.display(), shown);
        }
    }

    if counts_only {
        let mut rows: Vec<(&String, &usize)> = tally.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (value, count) in rows {
            println!("{}\t{}", count, value);
        }
    }
    eprintln!("{} file(s) scanned", files.len());
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("dcm"))
        {
            out.push(path);
        }
    }
    Ok(())
}

fn read_patient_id(path: &Path) -> Result<Option<String>, String> {
    let obj = open_file(path).map_err(|e| e.to_string())?;
    match obj.element(tags::PATIENT_ID) {
        Ok(elem) => elem
            .value()
            .to_str()
            .map(|v| Some(v.trim().to_string()))
            .map_err(|e| e.to_string()),
        Err(_) => Ok(None),
    }
}

/// Render a value with non-printable bytes escaped, so padding and
/// control characters are visible instead of silently blending in.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            c if c.is_control() || c == '\u{0}' => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
