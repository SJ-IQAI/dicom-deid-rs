//! Integration tests for `%filter allowlist` (r-2-10-3, r-5-2).
//!
//! These cover the structure every CTP institutional filter script uses: a
//! blanket rejection of a modality, with a whitelist of validated devices
//! admitted through it. The devices on such a whitelist are frequently ones
//! *known* to carry burned-in PHI — CTP's Stanford script annotates them
//! "-- SCRUBBED" — admitted on the understanding that the pixel masking rules
//! will remove it. So the load-bearing assertion here is not merely that an
//! allowlisted file survives, but that it is still masked and still has its
//! header de-identified. An allowlist that short-circuited the rest of the
//! pipeline would publish exactly the PHI the whitelist assumes is handled.

use dicom_dictionary_std::tags;
use dicom_object::open_file;
use std::collections::HashMap;
use std::fs;
use tempfile::TempDir;

use dicom_deid_rs::pipeline::{DeidConfig, DeidPipeline, DeidReport};

/// Fixture C from the graylist suite: ATL HDI 5000 ultrasound, 476x640.
const FIXTURE: &str = "us_atl_hdi5000.dcm";

/// Blanket rejection of ultrasound, the way the Stanford gauntlet rejects
/// `Modality.containsIgnoreCase("US")`.
const BLACKLIST: &str = "\
%filter blacklist

LABEL Reject ultrasound
contains Modality US
";

/// The device whitelist that admits this one validated scanner back through.
const ALLOWLIST: &str = "\
%filter allowlist

LABEL Admit validated ATL HDI 5000
contains Modality US
  + contains Manufacturer ATL
  + contains ManufacturerModelName HDI 5000
  + equals Rows 476
  + equals Columns 640
";

/// Masking rules and a header action, so a surviving file can be checked for
/// both. Coordinates match the ATL HDI 5000 label in complete-recipe.txt.
const GRAYLIST_AND_HEADER: &str = "\
%filter graylist

LABEL Mask ATL HDI 5000 annotation
contains Modality US
  + contains Manufacturer ATL
  + equals Rows 476
  + equals Columns 640
  + contains ManufacturerModelName HDI 5000
ctpcoordinates 40,0,200,40
ctpcoordinates 240,0,190,16

%header

REPLACE (0010,0020) ANON-ALLOWLIST  # PatientID
REMOVE (0008,0080)  # InstitutionName
";

/// Run the pipeline over the fixture with a recipe assembled from the given
/// sections. Returns the expected output path and the run report.
fn run_with_sections(sections: &[&str]) -> (std::path::PathBuf, DeidReport) {
    let fixture_path = format!("tests/fixtures/graylist/{FIXTURE}");
    assert!(
        std::path::Path::new(&fixture_path).exists(),
        "Fixture {fixture_path} not found. Run: cargo run --example gen_test_data --features jpeg2000",
    );

    let tmp = TempDir::new().expect("should create temp dir");
    let input_dir = tmp.path().join("input");
    let output_dir = tmp.path().join("output");
    fs::create_dir_all(&input_dir).expect("create input dir");
    fs::copy(&fixture_path, input_dir.join(FIXTURE)).expect("copy fixture");

    let recipe_path = tmp.path().join("recipe.txt");
    let recipe = format!("FORMAT dicom\n\n{}", sections.join("\n"));
    fs::write(&recipe_path, recipe).expect("write recipe");

    let config = DeidConfig {
        input_dir,
        output_dir: output_dir.clone(),
        recipe_path,
        variables: HashMap::new(),
        functions: HashMap::new(),
        salt: None,
        output_layout: None,
        mapping_file: None,
    };

    let pipeline = DeidPipeline::new(config).expect("should create pipeline");
    let report = pipeline.run().expect("should run pipeline");

    let output_path = output_dir.join(FIXTURE);
    // Leak the TempDir so it outlives the caller's reads of the output file.
    std::mem::forget(tmp);
    (output_path, report)
}

/// Assert that every pixel inside `regions` is zeroed and every pixel outside
/// them is untouched (the fixtures are solid white).
fn assert_masked(output_path: &std::path::Path, regions: &[(usize, usize, usize, usize)]) {
    let obj = open_file(output_path)
        .unwrap_or_else(|e| panic!("should open output {}: {e}", output_path.display()));
    let rows = obj
        .element(tags::ROWS)
        .expect("Rows")
        .to_int::<usize>()
        .expect("Rows as int");
    let cols = obj
        .element(tags::COLUMNS)
        .expect("Columns")
        .to_int::<usize>()
        .expect("Columns as int");
    let pixels = obj
        .element(tags::PIXEL_DATA)
        .expect("PixelData")
        .to_bytes()
        .expect("pixel bytes");

    let samples = pixels.len() / (rows * cols);
    assert!(samples >= 1, "expected at least one sample per pixel");

    let mut masked_seen = 0usize;
    for y in 0..rows {
        for x in 0..cols {
            let inside = regions
                .iter()
                .any(|(x0, y0, x1, y1)| x >= *x0 && x < *x1 && y >= *y0 && y < *y1);
            let value = pixels[(y * cols + x) * samples];
            if inside {
                assert_eq!(
                    value, 0,
                    "pixel ({x},{y}) inside a mask region must be zeroed"
                );
                masked_seen += 1;
            } else {
                assert_ne!(
                    value, 0,
                    "pixel ({x},{y}) outside every mask region must be untouched"
                );
            }
        }
    }
    assert!(masked_seen > 0, "expected at least one masked pixel");
}

/// Requirement r-5-1: the control. Without an allowlist the blanket rule
/// rejects the file, so the assertions below are known to be meaningful.
#[test]
fn r5_1_blacklist_alone_rejects_the_file() {
    let (output_path, report) = run_with_sections(&[BLACKLIST, GRAYLIST_AND_HEADER]);

    assert_eq!(
        report.files_blacklisted, 1,
        "the blanket rule must reject it"
    );
    assert_eq!(report.files_processed, 0);
    assert!(
        !output_path.exists(),
        "a blacklisted file must not appear in the output"
    );
}

/// Requirement r-5-2: adding an allowlist rule admits the same file.
#[test]
fn r5_2_allowlist_exempts_the_file_from_the_blacklist() {
    let (output_path, report) = run_with_sections(&[ALLOWLIST, BLACKLIST, GRAYLIST_AND_HEADER]);

    assert_eq!(
        report.files_blacklisted, 0,
        "the allowlist must exempt the file from the blanket rejection"
    );
    assert_eq!(report.files_processed, 1);
    assert!(
        output_path.exists(),
        "an exempt file must appear in the output"
    );
}

/// Requirement r-2-10-3: the exemption suppresses rejection and nothing else.
/// An allowlisted file must still be masked and still be de-identified.
#[test]
fn r2_10_3_allowlisted_file_is_still_masked_and_deidentified() {
    let (output_path, report) = run_with_sections(&[ALLOWLIST, BLACKLIST, GRAYLIST_AND_HEADER]);
    assert_eq!(report.files_processed, 1);

    assert_masked(&output_path, &[(40, 0, 240, 40), (240, 0, 430, 16)]);

    let obj = open_file(&output_path).expect("should open output");
    assert_eq!(
        obj.element(tags::PATIENT_ID)
            .expect("PatientID")
            .to_str()
            .expect("PatientID as str"),
        "ANON-ALLOWLIST",
        "header actions must still apply to an allowlisted file"
    );
    assert!(
        obj.element(tags::INSTITUTION_NAME).is_err(),
        "REMOVE must still apply to an allowlisted file"
    );
}

/// Requirement r-2-10-3: the allowlist only exempts what it actually matches.
/// A device absent from the whitelist stays rejected by the blanket rule.
#[test]
fn r2_10_3_allowlist_does_not_exempt_unlisted_devices() {
    let narrowed = "\
%filter allowlist

LABEL Admit some other scanner
contains Modality US
  + contains Manufacturer Aloka
";
    let (output_path, report) = run_with_sections(&[narrowed, BLACKLIST, GRAYLIST_AND_HEADER]);

    assert_eq!(
        report.files_blacklisted, 1,
        "an allowlist naming other devices must not admit this one"
    );
    assert!(!output_path.exists());
}

/// Requirement r-2-10-3: an allowlist with no blacklist alongside it changes
/// nothing — it grants no special treatment of its own.
#[test]
fn r2_10_3_allowlist_without_blacklist_is_inert() {
    let (with, with_report) = run_with_sections(&[ALLOWLIST, GRAYLIST_AND_HEADER]);
    let (without, without_report) = run_with_sections(&[GRAYLIST_AND_HEADER]);

    assert_eq!(with_report.files_processed, 1);
    assert_eq!(without_report.files_processed, 1);
    assert_eq!(
        fs::read(&with).expect("read allowlisted output"),
        fs::read(&without).expect("read plain output"),
        "an allowlist must not alter the de-identification of a file that was \
         never at risk of rejection"
    );
}
