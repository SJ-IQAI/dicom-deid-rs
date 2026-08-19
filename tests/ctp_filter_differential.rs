//! Differential test: the converted recipe must decide exactly as CTP does.
//!
//! `ctp_filter.txt` is produced by expanding one CTP boolean expression into
//! ~1350 flat labels via De Morgan and disjunctive normal form. That is a large
//! mechanical transformation, and reading the output is not a practical way to
//! gain confidence in it. So the decisions are compared instead.
//!
//! `tools/ctp_filter_diff_vectors.py` computes the reference answer by
//! interpreting the *parsed CTP expression tree* directly, with CTP's own
//! predicate semantics -- never via the conversion. Each vector is a synthetic
//! header plus that reference decision. Here every vector is replayed through
//! the real evaluator on the real recipe, and the two must agree:
//!
//!     allowlist matches OR blacklist does not match   ==   reference accept
//!
//! Regenerate both artifacts after changing either script or the converter:
//!
//!     tools/ctp_filter_to_recipe.py ctp_stanford.script \
//!         --graylist-from ctp_pixel.txt --output ctp_filter.txt
//!     tools/ctp_filter_diff_vectors.py ctp_stanford.script \
//!         --output tests/fixtures/ctp_filter_vectors.tsv

use std::collections::BTreeSet;

use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dicom_core::value::{DataSetSequence, PrimitiveValue, Value};
use dicom_core::{DataElement, Length, Tag, VR};
use dicom_dictionary_std::StandardDataDictionary;
use dicom_object::InMemDicomObject;

use dicom_deid_rs::filter_index::FilterIndex;
use dicom_deid_rs::recipe::Recipe;

const RECIPE_PATH: &str = "ctp_filter.txt";
const VECTORS_PATH: &str = "tests/fixtures/ctp_filter_vectors.tsv";

/// How many disagreements to print before truncating the failure message.
const MAX_REPORTED: usize = 15;

/// One reference decision: a synthetic header and whether CTP accepts it.
struct Vector {
    origin: String,
    accept: bool,
    /// (field, value). A field absent from this list models an absent element;
    /// an empty value models a present but empty one.
    fields: Vec<(String, String)>,
}

/// Parse the tab-separated fixture: `origin <TAB> accept|reject <TAB> Field=Value ...`
fn load_vectors() -> Vec<Vector> {
    let text = std::fs::read_to_string(VECTORS_PATH).unwrap_or_else(|e| {
        panic!(
            "{VECTORS_PATH} not readable ({e}). Regenerate with: \
             tools/ctp_filter_diff_vectors.py ctp_stanford.script --output {VECTORS_PATH}"
        )
    });

    text.lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let mut cells = line.split('\t');
            let origin = cells.next().expect("origin cell").to_string();
            let accept = match cells.next().expect("verdict cell") {
                "accept" => true,
                "reject" => false,
                other => panic!("unknown verdict {other:?}"),
            };
            let fields = cells
                .map(|cell| {
                    let (field, value) = cell.split_once('=').expect("Field=Value cell");
                    (field.to_string(), value.to_string())
                })
                .collect();
            Vector {
                origin,
                accept,
                fields,
            }
        })
        .collect()
}

fn tag_for(keyword: &str) -> Tag {
    StandardDataDictionary
        .by_name(keyword)
        .unwrap_or_else(|| panic!("{keyword} is not a DICOM keyword"))
        .tag()
}

/// A plausible VR for a synthetic element. Only affects how the value is
/// stored; the evaluator reads every field back as text.
fn vr_for(keyword: &str) -> VR {
    match keyword {
        "Rows" | "Columns" | "RegionDataType" => VR::US,
        "SeriesNumber" => VR::IS,
        "SOPClassUID" => VR::UI,
        "Modality" | "ImageType" | "BurnedInAnnotation" | "ConversionType" => VR::CS,
        "PixelSpacing" => VR::DS,
        _ => VR::LO,
    }
}

fn element(keyword: &str, value: &str) -> DataElement<InMemDicomObject> {
    let (tag, vr) = (tag_for(keyword), vr_for(keyword));
    if value.is_empty() {
        return DataElement::new(tag, vr, Value::Primitive(PrimitiveValue::Empty));
    }
    if vr == VR::US {
        let number: u16 = value
            .parse()
            .unwrap_or_else(|_| panic!("{keyword} needs a numeric value, got {value:?}"));
        return DataElement::new(tag, vr, Value::Primitive(PrimitiveValue::from(number)));
    }
    DataElement::new(tag, vr, Value::Primitive(PrimitiveValue::from(value)))
}

/// Build the DICOM object a vector describes. A `Seq::Element` field name
/// becomes a real single-item sequence, which is what r-2-6-11 resolves.
fn build_object(vector: &Vector) -> InMemDicomObject {
    let mut obj = InMemDicomObject::new_empty();
    for (field, value) in &vector.fields {
        match field.split_once("::") {
            Some((sequence, inner)) => {
                let mut item = InMemDicomObject::new_empty();
                item.put(element(inner, value));
                obj.put(DataElement::new(
                    tag_for(sequence),
                    VR::SQ,
                    Value::from(DataSetSequence::new(vec![item], Length::UNDEFINED)),
                ));
            }
            None => {
                obj.put(element(field, value));
            }
        }
    }
    obj
}

fn load_index() -> FilterIndex {
    let text = std::fs::read_to_string(RECIPE_PATH).expect("generated recipe must be readable");
    FilterIndex::new(&Recipe::parse(&text).expect("generated recipe must parse"))
}

fn describe(vector: &Vector) -> String {
    vector
        .fields
        .iter()
        .map(|(field, value)| format!("{field}={value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn verdict(accept: bool) -> &'static str {
    if accept { "accept" } else { "reject" }
}

/// Every vector's accept/reject decision must match the CTP reference.
#[test]
fn converted_recipe_decides_exactly_as_the_ctp_script() {
    let vectors = load_vectors();
    let index = load_index();
    assert!(
        vectors.len() > 10_000,
        "expected a substantial vector set, found {}",
        vectors.len()
    );

    let mut reported: Vec<String> = Vec::new();
    let mut mismatches = 0usize;
    let mut accepted = 0usize;

    for vector in &vectors {
        let obj = build_object(vector);

        // The pipeline's rule (r-5-2): an allowlist match exempts a file from
        // the blacklist; otherwise a blacklist match rejects it.
        let allowlisted = index.is_allowlisted(&obj);
        let blacklisted = index.blacklist_reason(&obj);
        let actual = allowlisted || blacklisted.is_none();

        if actual {
            accepted += 1;
        }
        if actual != vector.accept {
            mismatches += 1;
            if reported.len() < MAX_REPORTED {
                reported.push(format!(
                    "expected {} but got {} (allowlisted={allowlisted}, blacklist={:?})\n     origin={} fields: {}",
                    verdict(vector.accept),
                    verdict(actual),
                    blacklisted,
                    vector.origin,
                    describe(vector)
                ));
            }
        }
    }

    assert!(
        mismatches == 0,
        "{mismatches} of {} vectors disagree with the CTP reference\n\n  {}\n",
        vectors.len(),
        reported.join("\n\n  ")
    );

    // A harness that accepted everything, or rejected everything, would pass
    // the comparison above while testing nothing.
    assert!(
        accepted > 500 && accepted < vectors.len() - 500,
        "expected a mix of accepts and rejects, got {accepted} of {}",
        vectors.len()
    );
}

/// Each family of vectors must be present. The per-conjunction family is what
/// guarantees every label in the recipe is reached at least once.
#[test]
fn vector_families_cover_the_recipe() {
    let vectors = load_vectors();

    let origins: BTreeSet<&str> = vectors.iter().map(|v| v.origin.as_str()).collect();
    for wanted in ["satisfying:allow", "satisfying:block", "mutated", "random"] {
        assert!(
            origins.contains(wanted),
            "vector family {wanted:?} is missing; regenerate the fixture"
        );
    }

    let recipe_text = std::fs::read_to_string(RECIPE_PATH).expect("recipe readable");
    let labels = recipe_text
        .lines()
        .filter(|line| line.starts_with("LABEL "))
        .count();
    let satisfying = vectors
        .iter()
        .filter(|v| v.origin.starts_with("satisfying"))
        .count();
    assert!(
        satisfying * 3 > labels,
        "only {satisfying} satisfying vectors for {labels} labels; coverage has regressed"
    );
}

/// The allowlist must actually be doing work: some vectors have to be accepted
/// *because* of it, against a blacklist rule that matches them. If the
/// allowlist silently stopped matching, this fails -- whereas the comparison
/// test alone could still pass if the reference happened to agree.
#[test]
fn the_allowlist_rescues_files_the_blacklist_would_reject() {
    let vectors = load_vectors();
    let index = load_index();

    let rescued = vectors
        .iter()
        .filter(|vector| {
            let obj = build_object(vector);
            index.is_allowlisted(&obj) && index.blacklist_reason(&obj).is_some()
        })
        .count();

    assert!(
        rescued > 100,
        "only {rescued} vectors are admitted by the allowlist against a matching \
         blacklist rule; the two halves are supposed to overlap heavily, so this \
         suggests the allowlist has stopped matching"
    );
}
