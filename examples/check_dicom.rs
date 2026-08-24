//! Diagnose why `dicom_object::open_file` rejects a file, without
//! printing anything that could contain PHI: only byte offsets, hex of
//! the fixed-position structural bytes, and library error text.
//!
//! Usage:
//!   cargo run --example check_dicom -- <file-or-dir> [more paths...]
//!
//! For each `.dcm` file it reports:
//!   - file size
//!   - the first 16 bytes (hex) — catches git-lfs pointers, gzip, etc.
//!   - bytes 128..132, where the `DICM` magic must sit
//!   - whether `open_file` succeeds, and if not, the full error chain

use std::env;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("Usage: check_dicom <file-or-dir> [more paths...]");
        process::exit(1);
    }

    let mut files = Vec::new();
    for arg in &args {
        let path = PathBuf::from(arg);
        if path.is_dir() {
            if let Err(e) = collect(&path, &mut files) {
                eprintln!("Error reading {}: {}", path.display(), e);
            }
        } else {
            files.push(path);
        }
    }
    files.sort();

    let mut ok = 0usize;
    let mut bad = 0usize;
    for path in &files {
        if inspect(path) {
            ok += 1;
        } else {
            bad += 1;
        }
        println!();
    }
    eprintln!("{} readable, {} not readable", ok, bad);
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

/// Print structural facts about one file. Returns true if open_file
/// succeeded.
fn inspect(path: &Path) -> bool {
    println!("== {}", path.display());

    let mut head = [0u8; 160];
    let read = match File::open(path).and_then(|mut f| {
        let n = read_up_to(&mut f, &mut head)?;
        Ok((f.metadata()?.len(), n))
    }) {
        Ok(v) => v,
        Err(e) => {
            println!("   cannot read file: {}", e);
            return false;
        }
    };
    let (size, n) = read;
    println!("   size: {} bytes", size);
    println!("   first 16 bytes: {}", hex_ascii(&head[..n.min(16)]));

    if n >= 132 {
        let magic = &head[128..132];
        println!(
            "   bytes 128..132: {} {}",
            hex_ascii(magic),
            if magic == b"DICM" {
                "(DICM magic present)"
            } else {
                "(NOT the DICM magic)"
            }
        );
        if n >= 140 {
            // The first meta element should be (0002,0000) UL — shown as
            // raw hex so an endianness or offset problem is visible.
            println!("   bytes 132..140: {}", hex_ascii(&head[132..140]));
        }
    } else {
        println!("   file is shorter than 132 bytes — no room for a preamble + magic");
    }

    match dicom_object::open_file(path) {
        Ok(obj) => {
            println!(
                "   open_file: OK (transfer syntax {})",
                obj.meta().transfer_syntax()
            );
            true
        }
        Err(e) => {
            print!("   open_file: FAILED: {}", e);
            let mut source = std::error::Error::source(&e);
            while let Some(cause) = source {
                print!(": {}", cause);
                source = cause.source();
            }
            println!();
            false
        }
    }
}

fn read_up_to(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        let n = f.read(&mut buf[total..])?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

fn hex_ascii(bytes: &[u8]) -> String {
    let hex: Vec<String> = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    let ascii: String = bytes
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    format!("{} |{}|", hex.join(" "), ascii)
}
