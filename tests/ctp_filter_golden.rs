//! Golden tests for `ctp_filter.txt`: named cases a reviewer can check by hand.
//!
//! The differential harness (tests/ctp_filter_differential.rs) proves the
//! conversion agrees with the CTP script across ~39,000 generated headers, but
//! it proves agreement with the script *as written*, and it is unreadable as
//! documentation. These cases say in plain terms what the paired rules are for,
//! each traceable to a line of ctp_stanford.script, so a reviewer can confirm
//! the intent of the conversion and not merely its fidelity.

use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dicom_core::value::{DataSetSequence, PrimitiveValue, Value};
use dicom_core::{DataElement, Length, Tag, VR};
use dicom_dictionary_std::StandardDataDictionary;
use dicom_object::InMemDicomObject;

use dicom_deid_rs::filter_index::FilterIndex;
use dicom_deid_rs::recipe::Recipe;

const RECIPE_PATH: &str = "ctp_pixel_deid.txt";

fn index() -> FilterIndex {
    let text = std::fs::read_to_string(RECIPE_PATH).expect("generated recipe must be readable");
    FilterIndex::new(&Recipe::parse(&text).expect("generated recipe must parse"))
}

fn tag_for(keyword: &str) -> Tag {
    StandardDataDictionary
        .by_name(keyword)
        .unwrap_or_else(|| panic!("{keyword} is not a DICOM keyword"))
        .tag()
}

fn element(keyword: &str, value: &str) -> DataElement<InMemDicomObject> {
    let tag = tag_for(keyword);
    match keyword {
        "Rows" | "Columns" | "RegionDataType" => DataElement::new(
            tag,
            VR::US,
            Value::Primitive(PrimitiveValue::from(
                value.parse::<u16>().expect("numeric value"),
            )),
        ),
        _ => DataElement::new(tag, VR::LO, Value::Primitive(PrimitiveValue::from(value))),
    }
}

/// Build an object from `(keyword, value)` pairs. `Rows`/`Columns` are written
/// as the unsigned shorts they really are; a `Seq::Element` name becomes a real
/// one-item sequence.
fn object(fields: &[(&str, &str)]) -> InMemDicomObject {
    let mut obj = InMemDicomObject::new_empty();
    for (field, value) in fields {
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

/// The admission decision the pipeline makes (r-5-2).
fn accepted(index: &FilterIndex, obj: &InMemDicomObject) -> bool {
    index.is_allowlisted(obj) || index.blacklist_reason(obj).is_none()
}

fn assert_accepted(fields: &[(&str, &str)], why: &str) {
    let index = index();
    let obj = object(fields);
    assert!(
        accepted(&index, &obj),
        "expected this to be admitted ({why}), but it was rejected by {:?}",
        index.blacklist_reason(&obj)
    );
}

fn assert_rejected(fields: &[(&str, &str)], why: &str) {
    let index = index();
    let obj = object(fields);
    assert!(
        !accepted(&index, &obj),
        "expected this to be rejected ({why}), but it was admitted (allowlisted={})",
        index.is_allowlisted(&obj)
    );
}

// ---------------------------------------------------------------------------
// Whitelisted devices: admitted, and masked
// ---------------------------------------------------------------------------

/// ctp_stanford.script:11 — "KONICA 0402 CR -- SCRUBBED". That annotation means
/// the device carries burned-in PHI and is admitted anyway, because the pixel
/// rules mask it. Both halves are asserted here: admitting it without masking it
/// would publish that PHI.
#[test]
fn konica_0402_cr_is_admitted_and_masked() {
    let fields = [
        ("Manufacturer", "KONICA MINOLTA"),
        ("Modality", "CR"),
        ("ManufacturerModelName", "0402"),
        ("SoftwareVersions", "6.1"),
        ("Rows", "2446"),
        ("Columns", "2446"),
    ];
    assert_accepted(&fields, "a whitelisted KONICA 0402 CR");

    assert!(
        !index().get_graylist_regions(&object(&fields)).is_empty(),
        "a device the script marks SCRUBBED must receive mask regions; \
         admitting it unmasked would publish burned-in PHI"
    );
}

/// ctp_stanford.script:48 — GE CT+PET Discovery, also marked SCRUBBED.
#[test]
fn ge_discovery_pet_is_admitted_and_masked() {
    let fields = [
        ("Manufacturer", "GE MEDICAL SYSTEMS"),
        ("Modality", "PT"),
        ("ManufacturerModelName", "Discovery STE"),
        (
            "SecondaryCaptureDeviceManufacturerModelName",
            "Volume Viewer",
        ),
        ("SoftwareVersions", "5.2"),
        ("Rows", "512"),
        ("Columns", "512"),
    ];
    assert_accepted(&fields, "a whitelisted GE Discovery PET");
    assert!(
        !index().get_graylist_regions(&object(&fields)).is_empty(),
        "GE Discovery PET must receive mask regions"
    );
}

/// ctp_stanford.script:459 — Philips iU22 ultrasound at a validated resolution.
/// Ultrasound is rejected wholesale by the gauntlet (`:1077`), so this gets
/// through only because the device whitelist admits it back.
#[test]
fn philips_iu22_ultrasound_is_admitted_against_the_blanket_us_rejection() {
    let fields = [
        ("Manufacturer", "Philips Medical Systems"),
        ("Modality", "US"),
        ("ManufacturerModelName", "iU22"),
        ("SOPClassUID", "1.2.840.10008.5.1.4.1.1.6.1"),
        ("Rows", "768"),
        ("Columns", "1024"),
        ("ImageType", "ORIGINAL\\PRIMARY"),
        ("SequenceOfUltrasoundRegions::RegionDataType", "3"),
    ];
    assert_accepted(&fields, "a whitelisted Philips iU22");

    assert!(
        index().blacklist_reason(&object(&fields)).is_some(),
        "the blanket ultrasound rejection must still match it -- the allowlist \
         is what admits it, so if that rejection stopped matching, this test \
         would no longer prove anything"
    );
}

/// ctp_stanford.script:454 — the same scanner without the ultrasound region
/// sequence is a screenshot rather than an image, and is not admitted. This is
/// the rule needing `Seq::Element` resolution (r-2-6-11); without it the field
/// never resolves and the distinction silently disappears.
#[test]
fn philips_ultrasound_screenshot_without_region_sequence_is_rejected() {
    assert_rejected(
        &[
            ("Manufacturer", "Philips Medical Systems"),
            ("Modality", "US"),
            ("ManufacturerModelName", "iU22"),
            ("SOPClassUID", "1.2.840.10008.5.1.4.1.1.6.1"),
            ("Rows", "768"),
            ("Columns", "1024"),
            ("ImageType", "ORIGINAL\\PRIMARY"),
        ],
        "no ultrasound region sequence, so it is a screenshot",
    );
}

/// ctp_stanford.script:403 — ATL is deliberately absent from the whitelist:
/// its screenshots cannot be told apart from its images by any DICOM tag. Yet
/// ctp_pixel.txt *does* carry mask regions for it, under a label naming itself
/// a safety fallback.
///
/// So the two halves disagree on ATL, on purpose. The pixel library says "if
/// one turns up, here is roughly where the text sits"; the filter says "do not
/// let one turn up". Having a masking rule is not the same as being trusted,
/// and only the filter decides admission. Worth pinning down, because the
/// natural later question is why ATL studies are missing from the output when
/// mask regions for them plainly exist.
#[test]
fn atl_ultrasound_is_rejected_despite_having_mask_rules() {
    let fields = [
        ("Manufacturer", "ATL"),
        ("Modality", "US"),
        ("Rows", "476"),
        ("Columns", "640"),
    ];
    assert_rejected(&fields, "ATL ultrasound is not whitelisted");
    assert!(
        !index().get_graylist_regions(&object(&fields)).is_empty(),
        "the graylist does carry regions for this device; the point of this test \
         is that having them is not sufficient for admission"
    );
}

// ---------------------------------------------------------------------------
// MR, the modality most affected by adopting the gauntlet
// ---------------------------------------------------------------------------

/// A plain original MR is neither whitelisted nor caught by the gauntlet, so it
/// passes. Ordinary MR is unaffected by adopting this filter.
#[test]
fn original_primary_mr_passes() {
    assert_accepted(
        &[
            ("Manufacturer", "SIEMENS"),
            ("Modality", "MR"),
            ("ManufacturerModelName", "MAGNETOM Vida"),
            ("ImageType", "ORIGINAL\\PRIMARY\\M\\ND"),
        ],
        "an ordinary original MR image",
    );
}

/// ctp_stanford.script:1162 — derived MR is admitted only while it stays
/// DERIVED\PRIMARY. This is the clause that decides how much of an MR archive
/// survives the filter.
#[test]
fn derived_primary_mr_passes_but_other_derived_mr_does_not() {
    assert_accepted(
        &[
            ("Manufacturer", "SIEMENS"),
            ("Modality", "MR"),
            ("ImageType", "DERIVED\\PRIMARY\\M\\ND"),
        ],
        "DERIVED\\PRIMARY MR is explicitly allowed through",
    );
    assert_rejected(
        &[
            ("Manufacturer", "SIEMENS"),
            ("Modality", "MR"),
            ("ImageType", "DERIVED\\SECONDARY\\MPR"),
        ],
        "derived MR that is not DERIVED\\PRIMARY is rejected",
    );
}

// ---------------------------------------------------------------------------
// The gauntlet
// ---------------------------------------------------------------------------

/// ctp_stanford.script:1118 — SR objects carry narrative PHI.
#[test]
fn structured_report_objects_are_rejected() {
    assert_rejected(
        &[("Modality", "SR"), ("Manufacturer", "SIEMENS")],
        "SR is an excluded modality",
    );
}

/// ctp_stanford.script:1125 — encapsulated PDF.
#[test]
fn encapsulated_pdf_is_rejected() {
    assert_rejected(
        &[
            ("Modality", "OT"),
            ("SOPClassUID", "1.2.840.10008.5.1.4.1.1.104.1"),
        ],
        "encapsulated PDF",
    );
}

/// ctp_stanford.script:1153 — secondary capture.
#[test]
fn secondary_capture_is_rejected() {
    assert_rejected(
        &[
            ("Modality", "CT"),
            ("Manufacturer", "SIEMENS"),
            ("SOPClassUID", "1.2.840.10008.5.1.4.1.1.7"),
            ("ImageType", "ORIGINAL\\PRIMARY"),
        ],
        "secondary capture SOP class",
    );
}

/// ctp_stanford.script:1155 — the scanner itself has declared the pixels
/// annotated.
#[test]
fn burned_in_annotation_is_rejected() {
    assert_rejected(
        &[
            ("Modality", "CT"),
            ("Manufacturer", "SIEMENS"),
            ("ImageType", "ORIGINAL\\PRIMARY\\AXIAL"),
            ("BurnedInAnnotation", "YES"),
        ],
        "BurnedInAnnotation is YES",
    );
}

/// ctp_stanford.script:1154 — an absent ImageType is rejected exactly as an
/// empty one is. This is the CTP empty-value convention that `blank` models
/// (r-2-6-9). Converting it to `empty` instead would let files carrying no
/// ImageType at all slip through: the one mis-conversion here that fails open.
#[test]
fn missing_image_type_is_rejected_just_as_an_empty_one_is() {
    assert_rejected(
        &[
            ("Modality", "CT"),
            ("Manufacturer", "SIEMENS"),
            ("ImageType", ""),
        ],
        "empty ImageType",
    );
    assert_rejected(
        &[("Modality", "CT"), ("Manufacturer", "SIEMENS")],
        "absent ImageType, which CTP reads the same as an empty one",
    );
}

/// ctp_stanford.script:1132 — film scanners digitise paper, which carries PHI
/// that no tag describes.
#[test]
fn vidar_film_scanners_are_rejected() {
    assert_rejected(
        &[
            ("Modality", "CT"),
            ("Manufacturer", "VIDAR Systems"),
            ("ImageType", "ORIGINAL\\PRIMARY"),
        ],
        "Vidar film scanner",
    );
}

/// ctp_stanford.script:1144 — this PACS embeds scanned documents with no hint
/// but a high series number. It is the only rule using a regex,
/// `.matches("[1-9]\d{3,}")`, converted to an anchored `contains`.
#[test]
fn infinitt_high_series_numbers_are_rejected() {
    assert_rejected(
        &[
            ("Modality", "CT"),
            ("Manufacturer", "INFINITT Healthcare"),
            ("ImageType", "ORIGINAL\\PRIMARY"),
            ("SeriesNumber", "9001"),
        ],
        "a 4-digit series number from this PACS",
    );
    assert_accepted(
        &[
            ("Modality", "CT"),
            ("Manufacturer", "INFINITT Healthcare"),
            ("ImageType", "ORIGINAL\\PRIMARY"),
            ("SeriesNumber", "900"),
        ],
        "the same PACS with an ordinary series number is unaffected",
    );
}

/// A plain original CT is untouched by any of this.
#[test]
fn ordinary_ct_passes() {
    assert_accepted(
        &[
            ("Manufacturer", "SIEMENS"),
            ("Modality", "CT"),
            ("ManufacturerModelName", "SOMATOM Force"),
            ("ImageType", "ORIGINAL\\PRIMARY\\AXIAL"),
            ("Rows", "512"),
            ("Columns", "512"),
        ],
        "an ordinary original CT image",
    );
}
