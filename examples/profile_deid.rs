//! Time each stage of the de-identification pipeline on real files,
//! printing only stage timings and structural facts — no PHI.
//!
//! Usage:
//!   cargo run --release --example profile_deid -- <recipe> <file-or-dir> [--var NAME VALUE]... [--per-action]
//!
//! Runs every stage the pipeline runs, in the same order, and reports
//! per-stage wall time so the slow one is unambiguous.
//!
//! With --per-action, each recipe rule is additionally timed on its own
//! against a fresh copy of the object, and the slowest rules are
//! reported by tag number and action type (no element values are
//! printed).

use dicom_deid_rs::filter_index::FilterIndex;
use dicom_deid_rs::functions;
use dicom_deid_rs::metadata;
use dicom_deid_rs::recipe::Recipe;
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process;
use std::time::Instant;

fn main() {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let mut variables: HashMap<String, String> = HashMap::new();
    while let Some(pos) = args.iter().position(|a| a == "--var") {
        if pos + 2 >= args.len() {
            eprintln!("--var needs NAME and VALUE");
            process::exit(1);
        }
        let value = args.remove(pos + 2);
        let name = args.remove(pos + 1);
        args.remove(pos);
        variables.insert(name, value);
    }
    let per_action = if let Some(pos) = args.iter().position(|a| a == "--per-action") {
        args.remove(pos);
        true
    } else {
        false
    };
    if args.len() != 2 {
        eprintln!(
            "Usage: profile_deid <recipe> <file-or-dir> [--var NAME VALUE]... [--per-action]"
        );
        process::exit(1);
    }
    let recipe_path = PathBuf::from(&args[0]);
    let input = PathBuf::from(&args[1]);

    let t = Instant::now();
    let recipe_text = std::fs::read_to_string(&recipe_path).expect("read recipe");
    let recipe = Recipe::parse(&recipe_text).expect("parse recipe");
    println!(
        "recipe: {} header actions, {} filters, parsed in {:?}",
        recipe.header.len(),
        recipe.filters.len(),
        t.elapsed()
    );

    let t = Instant::now();
    let filter_index = FilterIndex::new(&recipe);
    println!("filter index built in {:?}", t.elapsed());

    let funcs = functions::default_functions(None);

    let mut files = Vec::new();
    if input.is_dir() {
        collect(&input, &mut files);
        files.sort();
    } else {
        files.push(input);
    }
    println!("{} file(s)\n", files.len());

    let out_dir = env::temp_dir().join("profile_deid_out");
    std::fs::create_dir_all(&out_dir).expect("create out dir");

    for path in &files {
        println!("== {}", path.display());
        let total = Instant::now();

        let t = Instant::now();
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let mut obj = match dicom_object::open_file(path) {
            Ok(o) => o,
            Err(e) => {
                println!("   open_file FAILED after {:?}: {}", t.elapsed(), e);
                continue;
            }
        };
        let n_elems = obj.iter().count();
        println!(
            "   open_file:            {:>12?}  ({} bytes, {} top-level elements, ts {})",
            t.elapsed(),
            size,
            n_elems,
            obj.meta().transfer_syntax()
        );

        let t = Instant::now();
        let allow = filter_index.is_allowlisted(&obj);
        let black = filter_index.blacklist_reason(&obj).map(str::to_string);
        println!(
            "   filter checks:        {:>12?}  (allowlisted={}, blacklisted={})",
            t.elapsed(),
            allow,
            black.is_some()
        );

        let t = Instant::now();
        let regions = filter_index.get_graylist_regions(&obj);
        println!(
            "   graylist match:       {:>12?}  ({} regions)",
            t.elapsed(),
            regions.len()
        );

        if per_action {
            let mut timings: Vec<(usize, std::time::Duration)> = Vec::new();
            for (i, action) in recipe.header.iter().enumerate() {
                let mut copy = obj.clone();
                let t = Instant::now();
                let result = metadata::apply_header_actions(
                    std::slice::from_ref(action),
                    &variables,
                    &funcs,
                    &mut copy,
                );
                let elapsed = t.elapsed();
                if let Err(e) = result {
                    println!("   rule {} FAILED: {}", i, e);
                }
                timings.push((i, elapsed));
            }
            timings.sort_by(|a, b| b.1.cmp(&a.1));
            println!("   slowest individual rules:");
            for (i, dur) in timings.iter().take(10) {
                let action = &recipe.header[*i];
                println!(
                    "     {:>12?}  rule {:>3}: {:?} {:?}",
                    dur, i, action.action_type, action.tag
                );
            }
        }

        let t = Instant::now();
        if let Err(e) = metadata::apply_header_actions(&recipe.header, &variables, &funcs, &mut obj)
        {
            println!("   header actions FAILED after {:?}: {}", t.elapsed(), e);
            continue;
        }
        println!("   header actions:       {:>12?}", t.elapsed());

        let t = Instant::now();
        metadata::remove_private_tags(&mut obj);
        println!("   remove private tags:  {:>12?}", t.elapsed());

        let t = Instant::now();
        if let Err(e) = metadata::sync_file_meta(&mut obj) {
            println!("   meta sync FAILED after {:?}: {}", t.elapsed(), e);
            continue;
        }
        println!("   meta sync:            {:>12?}", t.elapsed());

        let t = Instant::now();
        let out_path = out_dir.join(path.file_name().expect("file name"));
        match obj.write_to_file(&out_path) {
            Ok(()) => println!("   write_to_file:        {:>12?}", t.elapsed()),
            Err(e) => println!("   write_to_file FAILED after {:?}: {}", t.elapsed(), e),
        }
        let _ = std::fs::remove_file(&out_path);

        println!("   TOTAL:                {:>12?}\n", total.elapsed());
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect(&path, out);
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("dcm"))
        {
            out.push(path);
        }
    }
}
