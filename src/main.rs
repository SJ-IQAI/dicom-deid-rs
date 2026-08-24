use dicom_deid_rs::layout::DEID_PATH_LAYOUT;
use dicom_deid_rs::pipeline::{DeidConfig, DeidPipeline};
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::process;

fn print_usage(program: &str) {
    eprintln!(
        "Usage: {} <input_dir> <output_dir> <recipe_file> [OPTIONS]",
        program
    );
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --var NAME VALUE      Define a recipe variable (can be repeated)");
    eprintln!("  --salt VALUE          Salt mixed into the built-in hashuid function.");
    eprintln!("                        Use the same salt across runs to keep hashed");
    eprintln!("                        values consistent for a dataset.");
    eprintln!("  --mapper PATH         Take PatientID (0010,0020) from a .csv or .json");
    eprintln!("                        file of original-to-replacement pairs. The recipe");
    eprintln!("                        still does everything else it normally does;");
    eprintln!("                        --mapper overrides it for this one tag only.");
    eprintln!("                        The original PatientID is looked up before the");
    eprintln!("                        recipe runs; a file whose PatientID is missing,");
    eprintln!("                        empty, or absent from the mapper is skipped and");
    eprintln!("                        never written.");
    eprintln!("                        CSV: original PatientID, replacement PatientID;");
    eprintln!("                        a header row such as 'PatientID,DeidPatientID'");
    eprintln!("                        is recognized and skipped.");
    eprintln!("                        JSON: {{\"MRN001\": \"ANON-1\"}}, or an array of");
    eprintln!("                        [original, replacement] pairs, or an array of");
    eprintln!("                        objects keyed by those column names.");
    eprintln!("  --deid-paths          Name output files from de-identified tag values");
    eprintln!("                        instead of mirroring the input tree, using:");
    eprintln!("                          {}", DEID_PATH_LAYOUT);
    eprintln!("  --output-layout TMPL  Same, with a custom '/'-separated template.");
    eprintln!("                        {{Token}} placeholders name DICOM tags by keyword,");
    eprintln!("                        (gggg,eeee), or bare hex. Conflicts with");
    eprintln!("                        --deid-paths.");
    eprintln!("  --mapping-file PATH   Write a tab-separated original-path to");
    eprintln!("                        de-identified-path mapping. This file lists the");
    eprintln!("                        input paths, so it is PHI: it must be outside");
    eprintln!("                        <output_dir> and stored accordingly.");
    eprintln!();
    eprintln!("Input paths are usually named after the identifiers being removed, so");
    eprintln!("--deid-paths is recommended whenever the output tree must be free of PHI.");
    eprintln!("The blacklist report is written to the current working directory.");
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut positional: Vec<String> = Vec::new();
    let mut variables: HashMap<String, String> = HashMap::new();
    let mut salt: Option<String> = None;
    let mut output_layout: Option<String> = None;
    let mut deid_paths = false;
    let mut mapping_file: Option<PathBuf> = None;
    let mut mapper_file: Option<PathBuf> = None;

    let fail = |message: &str, program: &str| -> ! {
        eprintln!("Error: {}", message);
        print_usage(program);
        process::exit(1);
    };

    let mut i = 1;
    while i < args.len() {
        // Options may appear before or after the positional arguments,
        // since --mapper makes the third positional optional.
        match args[i].as_str() {
            "--var" => {
                if i + 2 >= args.len() {
                    fail("--var requires NAME and VALUE arguments", &args[0]);
                }
                variables.insert(args[i + 1].clone(), args[i + 2].clone());
                i += 3;
            }
            "--salt" => {
                if i + 1 >= args.len() {
                    fail("--salt requires a VALUE argument", &args[0]);
                }
                salt = Some(args[i + 1].clone());
                i += 2;
            }
            "--mapper" => {
                if i + 1 >= args.len() {
                    fail("--mapper requires a PATH argument", &args[0]);
                }
                mapper_file = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--output-layout" => {
                if i + 1 >= args.len() {
                    fail("--output-layout requires a TEMPLATE argument", &args[0]);
                }
                output_layout = Some(args[i + 1].clone());
                i += 2;
            }
            "--deid-paths" => {
                deid_paths = true;
                i += 1;
            }
            "--mapping-file" => {
                if i + 1 >= args.len() {
                    fail("--mapping-file requires a PATH argument", &args[0]);
                }
                mapping_file = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--help" | "-h" => {
                print_usage(&args[0]);
                process::exit(0);
            }
            other if other.starts_with("--") => {
                fail(&format!("unknown argument '{}'", other), &args[0]);
            }
            other => {
                positional.push(other.to_string());
                i += 1;
            }
        }
    }

    if positional.len() != 3 {
        fail(
            "expected <input_dir>, <output_dir> and <recipe_file>",
            &args[0],
        );
    }

    if deid_paths {
        if output_layout.is_some() {
            fail(
                "--deid-paths and --output-layout are mutually exclusive",
                &args[0],
            );
        }
        output_layout = Some(DEID_PATH_LAYOUT.to_string());
    }

    let config = DeidConfig {
        input_dir: PathBuf::from(&positional[0]),
        output_dir: PathBuf::from(&positional[1]),
        recipe_path: PathBuf::from(&positional[2]),
        variables,
        functions: HashMap::new(),
        salt,
        output_layout,
        mapping_file,
    };

    let pipeline = match &mapper_file {
        Some(path) => DeidPipeline::with_mapper_file(config, path),
        None => DeidPipeline::new(config),
    };
    let pipeline = match pipeline {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error initializing pipeline: {}", e);
            process::exit(1);
        }
    };

    match pipeline.run() {
        Ok(report) => {
            println!("De-identification complete:");
            println!("  Files processed:  {}", report.files_processed);
            println!("  Files blacklisted: {}", report.files_blacklisted);
            println!("  Files skipped:    {}", report.files_skipped);
            if let Some(path) = &report.blacklist_report_path {
                println!("  Blacklist report: {}", path.display());
            }
            if let Some(path) = &report.mapping_file_path {
                println!("  Path mapping:     {}", path.display());
            }
        }
        Err(e) => {
            eprintln!("Error running pipeline: {}", e);
            process::exit(1);
        }
    }
}
