//! PatientID mapper files (r-7).
//!
//! A mapper file is a table of original PatientID to replacement
//! PatientID. Supplying one puts the tool in *mapper mode* (r-7-2),
//! where the only de-identification applied to the data set is the
//! PatientID substitution — no recipe actions, no filters, no pixel
//! masking, no private tag removal. It exists for the case where a site
//! has already assigned study identifiers out of band and wants nothing
//! else about the file disturbed.
//!
//! Because the substitution is the *only* protection in this mode, a
//! value with no entry in the mapper is never passed through: the file
//! is reported and skipped (r-7-6), so nothing leaves the tool carrying
//! an original PatientID.

use crate::error::DeidError;
use crate::json::{self, JsonValue};
use dicom_core::header::Header;
use dicom_core::value::{PrimitiveValue, Value};
use dicom_core::{DataElement, Tag, VR};
use dicom_dictionary_std::tags;
use dicom_object::InMemDicomObject;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs;
use std::path::Path;

/// PatientID (0010,0020) is VR LO, which PS3.5 caps at 64 bytes. A
/// longer replacement could not be written without truncation, so it is
/// rejected when the mapper is loaded rather than corrupting output.
const LO_MAX_BYTES: usize = 64;

/// Column headings accepted for the original PatientID (r-7-3),
/// normalized by [`normalize_heading`].
const SOURCE_HEADINGS: &[&str] = &[
    "patientid",
    "originalpatientid",
    "originalid",
    "original",
    "source",
    "sourceid",
    "from",
    "old",
    "oldpatientid",
    "id",
    "key",
    "mrn",
];

/// Column headings accepted for the replacement PatientID (r-7-3).
const TARGET_HEADINGS: &[&str] = &[
    "deidpatientid",
    "deidentifiedpatientid",
    "newpatientid",
    "anonpatientid",
    "anonymizedpatientid",
    "mappedpatientid",
    "deid",
    "deidentified",
    "anon",
    "anonymized",
    "new",
    "newid",
    "mapped",
    "mappedid",
    "replacement",
    "target",
    "to",
    "value",
];

/// A lookup table from original PatientID to replacement PatientID.
#[derive(Debug, Clone, Default)]
pub struct PatientIdMapper {
    entries: HashMap<String, String>,
}

impl PatientIdMapper {
    /// Load a mapper file, choosing the parser from its extension
    /// (r-7-1): `.csv` and `.json` are supported.
    pub fn load(path: &Path) -> Result<Self, DeidError> {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();

        let text = fs::read_to_string(path).map_err(|e| {
            DeidError::Mapper(format!("cannot read mapper file {}: {}", path.display(), e))
        })?;

        let mapper = match extension.as_str() {
            "csv" => Self::from_csv(&text),
            "json" => Self::from_json(&text),
            "" => Err(DeidError::Mapper(format!(
                "mapper file {} has no extension; it must be .csv or .json",
                path.display()
            ))),
            other => Err(DeidError::Mapper(format!(
                "unsupported mapper file type '.{}' for {}; it must be .csv or .json",
                other,
                path.display()
            ))),
        };

        // Name the offending file in every parse error: the pipeline
        // reports this before any DICOM file is touched, with no other
        // context to identify it by.
        mapper.map_err(|e| match e {
            DeidError::Mapper(message) => {
                DeidError::Mapper(format!("{}: {}", path.display(), message))
            }
            other => other,
        })
    }

    /// Parse a CSV mapper file (r-7-3).
    ///
    /// The first column holds the original PatientID and the second the
    /// replacement; further columns are ignored. A leading header row is
    /// detected and skipped when its first cell is a recognized heading,
    /// in which case the replacement column is located by heading too,
    /// so a file whose columns are the other way round still loads
    /// correctly.
    pub fn from_csv(text: &str) -> Result<Self, DeidError> {
        let records = parse_csv(text)?;
        let mut rows = records.iter().enumerate();

        let (source_column, target_column) = match records.first() {
            Some(first) if is_heading_row(first) => {
                rows.next(); // consume the header row
                heading_columns(first)
            }
            _ => (0, 1),
        };
        let widest = source_column.max(target_column);

        let mut entries = Entries::new();
        for (index, record) in rows {
            let line = index + 1;
            if record.len() <= widest {
                return Err(DeidError::Mapper(format!(
                    "row {} has {} column(s); at least {} are needed to read an \
                     original and a replacement PatientID",
                    line,
                    record.len(),
                    widest + 1
                )));
            }
            entries.insert(
                &record[source_column],
                &record[target_column],
                &format!("row {}", line),
            )?;
        }

        entries.finish()
    }

    /// Parse a JSON mapper file (r-7-4).
    ///
    /// Three shapes are accepted:
    ///
    /// - an object of pairs: `{"MRN0012345": "ANON-0001"}`
    /// - an array of two-element arrays: `[["MRN0012345", "ANON-0001"]]`
    /// - an array of objects keyed by column heading:
    ///   `[{"PatientID": "MRN0012345", "DeidPatientID": "ANON-0001"}]`
    pub fn from_json(text: &str) -> Result<Self, DeidError> {
        let value = json::parse(text)
            .map_err(|e| DeidError::Mapper(format!("mapper file is not valid JSON: {}", e)))?;

        let mut entries = Entries::new();
        match value {
            // A top-level object is always the map itself. Deciding
            // otherwise for objects whose keys happen to look like
            // column headings would make the meaning of a file depend on
            // the identifiers inside it.
            JsonValue::Object(members) => {
                for (key, value) in &members {
                    let replacement = json_scalar(value)
                        .ok_or_else(|| json_type_error(&format!("value for \"{}\"", key), value))?;
                    entries.insert(key, &replacement, &format!("entry \"{}\"", key))?;
                }
            }
            JsonValue::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    let location = format!("entry {}", index + 1);
                    let (key, replacement) = json_record(item, &location)?;
                    entries.insert(&key, &replacement, &location)?;
                }
            }
            other => {
                return Err(json_type_error("mapper file", &other));
            }
        }

        entries.finish()
    }

    /// Build a mapper from pairs already in memory, for callers
    /// embedding the library (r-6-2). The same validation as a parsed
    /// file applies.
    pub fn from_pairs<I, K, V>(pairs: I) -> Result<Self, DeidError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut entries = Entries::new();
        for (key, value) in pairs {
            let key = key.as_ref();
            entries.insert(key, value.as_ref(), &format!("entry \"{}\"", key))?;
        }
        entries.finish()
    }

    /// The number of pairs in the mapper.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the mapper holds no pairs. A loaded mapper never is —
    /// an empty file is rejected (r-7-1) — but the constructors are
    /// public, so the accessor is provided alongside [`Self::len`].
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The replacement for an original PatientID, if the mapper has one.
    ///
    /// Matching is exact on the trimmed value: DICOM pads values to an
    /// even length, so the padding is removed before the lookup, but
    /// nothing else about the identifier is normalized. Two identifiers
    /// that differ only in case are different identifiers.
    pub fn lookup(&self, patient_id: &str) -> Option<&str> {
        self.entries.get(patient_id.trim()).map(String::as_str)
    }

    /// Look up the replacement for a data set's PatientID (r-7-6).
    ///
    /// Called *before* the recipe runs, since the recipe is what changes
    /// the value being looked up. The data set's own top-level PatientID
    /// is what gets looked up, and only that: a file routinely carries
    /// PatientID copies inside sequences formatted differently from the
    /// top-level one (zero-padded to a fixed width, for instance), and
    /// treating each as its own identifier meant one unrecognized copy
    /// discarded a file whose actual PatientID mapped perfectly well.
    ///
    /// # Errors
    ///
    /// Returns [`DeidError::MapperLookup`] — non-fatal, so the pipeline
    /// counts the file as skipped per r-1-5 — when the data set carries
    /// no top-level PatientID, when it is empty, or when its value has
    /// no entry in the mapper.
    pub fn resolve_for(&self, obj: &InMemDicomObject) -> Result<String, DeidError> {
        let current = match obj.element(tags::PATIENT_ID) {
            Ok(elem) => elem.value().to_str().map_err(|e| {
                DeidError::MapperLookup(format!("cannot read PatientID (0010,0020): {}", e))
            })?,
            Err(_) => {
                return Err(DeidError::MapperLookup(
                    "data set has no PatientID (0010,0020) to map".to_string(),
                ));
            }
        };
        self.resolve(current.trim()).map(str::to_string)
    }

    /// Look up and apply in one step, for callers driving the mapper
    /// directly rather than through the pipeline (r-6-1).
    pub fn apply(&self, obj: &mut InMemDicomObject) -> Result<(), DeidError> {
        let replacement = self.resolve_for(obj)?;
        force_patient_id(obj, &replacement);
        Ok(())
    }

    /// Look up one value, turning a miss into the error that skips the
    /// file (r-7-6).
    fn resolve(&self, current: &str) -> Result<&str, DeidError> {
        if current.is_empty() {
            return Err(DeidError::MapperLookup(
                "data set has an empty PatientID (0010,0020) to map".to_string(),
            ));
        }
        self.entries
            .get(current)
            .map(String::as_str)
            .ok_or_else(|| {
                DeidError::MapperLookup(format!(
                    "PatientID '{}' has no entry in the mapper file",
                    current
                ))
            })
    }
}

/// Write the mapped value into PatientID, overriding whatever the
/// recipe left there (r-7-2).
///
/// Runs *after* the recipe, so the mapper wins no matter what the recipe
/// did to (0010,0020) — including removing it, in which case it is put
/// back. This is the single tag the mapper overrides; every other tag is
/// left exactly as the recipe produced it.
pub fn force_patient_id(obj: &mut InMemDicomObject, replacement: &str) {
    put_patient_id(obj, replacement);
    replace_nested(obj, replacement);
}

/// PatientID's dictionary VR, not whatever the source file declared: a
/// value written back as UN would not read as text on the far side.
fn put_patient_id(obj: &mut InMemDicomObject, replacement: &str) {
    obj.put(DataElement::new(
        tags::PATIENT_ID,
        VR::LO,
        Value::Primitive(PrimitiveValue::from(replacement)),
    ));
}

/// Write `replacement` over every PatientID nested inside sequences, at
/// any depth (r-7-7).
///
/// Only existing elements are overwritten; no PatientID is introduced
/// into a sequence item that did not already carry one.
fn replace_nested(obj: &mut InMemDicomObject, replacement: &str) {
    let sequences: Vec<Tag> = obj
        .iter()
        .filter(|elem| elem.value().items().is_some())
        .map(|elem| elem.tag())
        .collect();

    for tag in sequences {
        let mut elem = match obj.take_element(tag) {
            Ok(elem) => elem,
            Err(_) => continue,
        };
        elem.update_value(|value| {
            if let Some(items) = value.items_mut() {
                for item in items.iter_mut() {
                    if item.element(tags::PATIENT_ID).is_ok() {
                        put_patient_id(item, replacement);
                    }
                    replace_nested(item, replacement);
                }
            }
        });
        obj.put(elem);
    }
}

/// Accumulates validated pairs, rejecting conflicting duplicates.
struct Entries {
    entries: HashMap<String, String>,
}

impl Entries {
    fn new() -> Self {
        Entries {
            entries: HashMap::new(),
        }
    }

    /// Validate and record one pair (r-7-5).
    fn insert(&mut self, key: &str, value: &str, location: &str) -> Result<(), DeidError> {
        let key = key.trim();
        let value = value.trim();

        if key.is_empty() {
            return Err(DeidError::Mapper(format!(
                "{} has an empty original PatientID",
                location
            )));
        }
        // An empty replacement would blank PatientID rather than
        // pseudonymize it, which is a silent data loss, not a mapping.
        if value.is_empty() {
            return Err(DeidError::Mapper(format!(
                "{} has an empty replacement PatientID",
                location
            )));
        }
        if value.len() > LO_MAX_BYTES {
            return Err(DeidError::Mapper(format!(
                "{} has a replacement PatientID of {} bytes; PatientID (0010,0020) is \
                 VR LO, which allows at most {}",
                location,
                value.len(),
                LO_MAX_BYTES
            )));
        }
        // A backslash separates values in a multi-valued element, so one
        // inside a replacement would be read back as two PatientIDs.
        if value.contains('\\') {
            return Err(DeidError::Mapper(format!(
                "{} has a replacement PatientID containing a backslash, which DICOM \
                 reads as a value separator",
                location
            )));
        }
        if value.chars().any(|c| c.is_control()) {
            return Err(DeidError::Mapper(format!(
                "{} has a replacement PatientID containing a control character",
                location
            )));
        }

        match self.entries.entry(key.to_string()) {
            Entry::Occupied(existing) => {
                // Repeating a pair is harmless — mapper files are often
                // generated per series and concatenated. Two different
                // replacements for one identifier are not: silently
                // picking one would make output depend on file order.
                if existing.get() != value {
                    return Err(DeidError::Mapper(format!(
                        "{} maps PatientID '{}' to '{}', but it is already mapped to '{}'",
                        location,
                        key,
                        value,
                        existing.get()
                    )));
                }
            }
            Entry::Vacant(slot) => {
                slot.insert(value.to_string());
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<PatientIdMapper, DeidError> {
        if self.entries.is_empty() {
            return Err(DeidError::Mapper(
                "mapper file contains no PatientID pairs".to_string(),
            ));
        }
        Ok(PatientIdMapper {
            entries: self.entries,
        })
    }
}

/// A JSON scalar as the text to use for a PatientID, or `None` when the
/// value is a composite or null. Numbers keep their source text, so a
/// numeric MRN is not reformatted.
fn json_scalar(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::String(text) => Some(text.clone()),
        JsonValue::Number(text) => Some(text.clone()),
        _ => None,
    }
}

fn json_type_error(what: &str, value: &JsonValue) -> DeidError {
    DeidError::Mapper(format!(
        "{} is {}; a mapper file must be an object of PatientID pairs, an array of \
         [original, replacement] pairs, or an array of objects keyed by column name",
        what,
        value.type_name()
    ))
}

/// Read one record out of a JSON array: either `[original, replacement]`
/// or an object keyed by column heading.
fn json_record(value: &JsonValue, location: &str) -> Result<(String, String), DeidError> {
    match value {
        JsonValue::Array(pair) => {
            if pair.len() != 2 {
                return Err(DeidError::Mapper(format!(
                    "{} has {} element(s); an array entry must be \
                     [original, replacement]",
                    location,
                    pair.len()
                )));
            }
            let key = json_scalar(&pair[0]).ok_or_else(|| {
                json_type_error(&format!("{}'s original PatientID", location), &pair[0])
            })?;
            let replacement = json_scalar(&pair[1]).ok_or_else(|| {
                json_type_error(&format!("{}'s replacement PatientID", location), &pair[1])
            })?;
            Ok((key, replacement))
        }
        JsonValue::Object(members) => {
            let find = |headings: &[&str]| {
                members
                    .iter()
                    .find(|(name, _)| headings.contains(&normalize_heading(name).as_str()))
            };
            let (source_name, source) = find(SOURCE_HEADINGS).ok_or_else(|| {
                DeidError::Mapper(format!(
                    "{} has no member naming the original PatientID (expected one of: {})",
                    location,
                    SOURCE_HEADINGS.join(", ")
                ))
            })?;
            let (target_name, target) = find(TARGET_HEADINGS).ok_or_else(|| {
                DeidError::Mapper(format!(
                    "{} has no member naming the replacement PatientID (expected one of: {})",
                    location,
                    TARGET_HEADINGS.join(", ")
                ))
            })?;
            let key = json_scalar(source).ok_or_else(|| {
                json_type_error(&format!("{}'s \"{}\"", location, source_name), source)
            })?;
            let replacement = json_scalar(target).ok_or_else(|| {
                json_type_error(&format!("{}'s \"{}\"", location, target_name), target)
            })?;
            Ok((key, replacement))
        }
        other => Err(json_type_error(location, other)),
    }
}

/// Fold a column heading to its comparable form: lowercase, with
/// spaces, underscores, and punctuation dropped. This is what lets
/// `PatientID`, `patient_id`, and `Patient ID` all be one heading.
fn normalize_heading(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Whether a CSV record is a header row rather than data.
///
/// Only the first cell is consulted, and either kind of heading counts,
/// so a file whose columns are written the other way round is still
/// recognized. Looking further along the row would misread a data row
/// whose replacement identifier happened to spell a heading.
fn is_heading_row(record: &[String]) -> bool {
    record.first().is_some_and(|cell| {
        let heading = normalize_heading(cell);
        SOURCE_HEADINGS.contains(&heading.as_str()) || TARGET_HEADINGS.contains(&heading.as_str())
    })
}

/// The (original, replacement) column indices named by a header row,
/// falling back to the first two columns.
fn heading_columns(record: &[String]) -> (usize, usize) {
    let column_of = |headings: &[&str]| {
        record
            .iter()
            .position(|cell| headings.contains(&normalize_heading(cell).as_str()))
    };
    let source = column_of(SOURCE_HEADINGS).unwrap_or(0);
    let target = match column_of(TARGET_HEADINGS) {
        // A heading may not name both columns; if the replacement
        // heading landed on the original's column, it is not a usable
        // answer.
        Some(column) if column != source => column,
        _ if source != 1 => 1,
        _ => 0,
    };
    (source, target)
}

/// Split CSV text into records (r-7-3).
///
/// Follows RFC 4180: fields are comma separated, may be double-quoted,
/// and a quoted field escapes a quote by doubling it. Records end at
/// LF or CRLF, and blank lines are ignored so a trailing newline (or a
/// file pasted together from several exports) parses cleanly.
fn parse_csv(text: &str) -> Result<Vec<Vec<String>>, DeidError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let bytes = text.as_bytes();

    let mut records: Vec<Vec<String>> = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut index = 0;

    // Records are counted for error messages; a quoted field may span
    // lines, so this tracks records rather than physical lines.
    let mut record_number = 1;

    while index < bytes.len() {
        let byte = bytes[index];
        if quoted {
            match byte {
                b'"' => {
                    if bytes.get(index + 1) == Some(&b'"') {
                        field.push('"');
                        index += 2;
                    } else {
                        quoted = false;
                        index += 1;
                    }
                }
                _ => {
                    // Copy the whole UTF-8 character, not one byte of it.
                    let char_len = utf8_len(byte);
                    field.push_str(&text[index..index + char_len]);
                    index += char_len;
                }
            }
            continue;
        }

        match byte {
            b'"' => {
                if !field.is_empty() {
                    return Err(DeidError::Mapper(format!(
                        "row {} has a quote in the middle of an unquoted field",
                        record_number
                    )));
                }
                quoted = true;
                index += 1;
            }
            b',' => {
                record.push(std::mem::take(&mut field));
                index += 1;
            }
            b'\r' | b'\n' => {
                if byte == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
                index += 1;
                record.push(std::mem::take(&mut field));
                push_record(&mut records, std::mem::take(&mut record));
                record_number += 1;
            }
            _ => {
                let char_len = utf8_len(byte);
                field.push_str(&text[index..index + char_len]);
                index += char_len;
            }
        }
    }

    if quoted {
        return Err(DeidError::Mapper(format!(
            "row {} ends inside a quoted field",
            record_number
        )));
    }
    // A file not ending in a newline still has a final record.
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        push_record(&mut records, record);
    }

    Ok(records)
}

/// Add a record unless it is a blank line.
fn push_record(records: &mut Vec<Vec<String>>, record: Vec<String>) {
    if record.iter().all(|field| field.trim().is_empty()) {
        return;
    }
    records.push(record);
}

/// The length in bytes of the UTF-8 character starting with this byte.
fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;

    fn mapper(pairs: &[(&str, &str)]) -> PatientIdMapper {
        PatientIdMapper::from_pairs(pairs.iter().copied()).expect("should build mapper")
    }

    fn patient_id(obj: &InMemDicomObject) -> String {
        obj.element(tags::PATIENT_ID)
            .expect("should have PatientID")
            .value()
            .to_str()
            .expect("should read PatientID")
            .trim()
            .to_string()
    }

    fn obj_with_patient_id(value: &str) -> InMemDicomObject {
        let mut obj = InMemDicomObject::new_empty();
        obj.put(DataElement::new(
            tags::PATIENT_ID,
            VR::LO,
            Value::Primitive(PrimitiveValue::from(value)),
        ));
        obj
    }

    // -- r-7-3: CSV ----------------------------------------------------------

    /// Requirement r-7-3
    #[test]
    fn r7_3_reads_a_headerless_two_column_file() {
        let mapper =
            PatientIdMapper::from_csv("MRN001,ANON-1\nMRN002,ANON-2\n").expect("should parse");
        assert_eq!(mapper.len(), 2);
        assert_eq!(mapper.lookup("MRN001"), Some("ANON-1"));
        assert_eq!(mapper.lookup("MRN002"), Some("ANON-2"));
    }

    /// Requirement r-7-3
    #[test]
    fn r7_3_skips_a_recognized_header_row() {
        let mapper =
            PatientIdMapper::from_csv("PatientID,DeidPatientID\nMRN001,ANON-1\n").expect("parses");
        assert_eq!(mapper.len(), 1, "the header row must not become an entry");
        assert_eq!(mapper.lookup("MRN001"), Some("ANON-1"));
    }

    /// Requirement r-7-3: headings differing only in case, spacing, or
    /// punctuation are the same heading.
    #[test]
    fn r7_3_header_matching_ignores_case_and_punctuation() {
        for header in [
            "PatientID,DeidPatientID",
            "patient_id,deid_patient_id",
            "Patient ID,New Patient ID",
            "PATIENTID,REPLACEMENT",
            "  patientid  ,  new  ",
        ] {
            let text = format!("{}\nMRN001,ANON-1\n", header);
            let mapper = PatientIdMapper::from_csv(&text)
                .unwrap_or_else(|e| panic!("should parse header {:?}: {}", header, e));
            assert_eq!(mapper.len(), 1, "header {:?}", header);
            assert_eq!(
                mapper.lookup("MRN001"),
                Some("ANON-1"),
                "header {:?}",
                header
            );
        }
    }

    /// Requirement r-7-3: a header row locates the columns by name, so a
    /// file written in the other order still loads correctly.
    #[test]
    fn r7_3_header_row_locates_columns_by_name() {
        let mapper =
            PatientIdMapper::from_csv("DeidPatientID,PatientID\nANON-1,MRN001\n").expect("parses");
        assert_eq!(mapper.lookup("MRN001"), Some("ANON-1"));
    }

    /// Requirement r-7-3: columns beyond the pair are ignored.
    #[test]
    fn r7_3_extra_columns_are_ignored() {
        let mapper =
            PatientIdMapper::from_csv("PatientID,DeidPatientID,Site\nMRN001,ANON-1,Memphis\n")
                .expect("parses");
        assert_eq!(mapper.lookup("MRN001"), Some("ANON-1"));
    }

    /// Requirement r-7-3: RFC 4180 quoting.
    #[test]
    fn r7_3_parses_quoted_fields() {
        let mapper = PatientIdMapper::from_csv("\"MRN,001\",\"ANON \"\"1\"\"\"\n").expect("parses");
        assert_eq!(mapper.lookup("MRN,001"), Some("ANON \"1\""));
    }

    /// Requirement r-7-3: a quoted field may span lines.
    #[test]
    fn r7_3_quoted_field_may_span_lines() {
        let records = parse_csv("\"a\nb\",c\n").expect("parses");
        assert_eq!(records, vec![vec!["a\nb".to_string(), "c".to_string()]]);
    }

    /// Requirement r-7-3: CRLF line endings and blank lines.
    #[test]
    fn r7_3_handles_crlf_blank_lines_and_a_missing_trailing_newline() {
        let mapper =
            PatientIdMapper::from_csv("MRN001,ANON-1\r\n\r\nMRN002,ANON-2").expect("parses");
        assert_eq!(mapper.len(), 2);
        assert_eq!(mapper.lookup("MRN002"), Some("ANON-2"));
    }

    /// Requirement r-7-3: a byte order mark must not become part of the
    /// first identifier — Excel writes one on every CSV export.
    #[test]
    fn r7_3_strips_a_byte_order_mark() {
        let mapper = PatientIdMapper::from_csv("\u{feff}MRN001,ANON-1\n").expect("parses");
        assert_eq!(mapper.lookup("MRN001"), Some("ANON-1"));
    }

    /// Requirement r-7-3: surrounding whitespace is not part of an
    /// identifier.
    #[test]
    fn r7_3_trims_surrounding_whitespace() {
        let mapper = PatientIdMapper::from_csv("  MRN001 , ANON-1 \n").expect("parses");
        assert_eq!(mapper.lookup("MRN001"), Some("ANON-1"));
    }

    /// Requirement r-7-3: multi-byte characters survive parsing.
    #[test]
    fn r7_3_preserves_multibyte_characters() {
        let mapper = PatientIdMapper::from_csv("MÜLLER☂,ANON-1\n").expect("parses");
        assert_eq!(mapper.lookup("MÜLLER☂"), Some("ANON-1"));
    }

    /// Requirement r-7-3: a row too short to hold a pair is an error,
    /// not a silently dropped patient.
    #[test]
    fn r7_3_rejects_a_row_with_too_few_columns() {
        let err = PatientIdMapper::from_csv("MRN001,ANON-1\nMRN002\n")
            .expect_err("should reject the short row");
        assert!(
            err.to_string().contains("row 2"),
            "unexpected error: {}",
            err
        );
    }

    /// Requirement r-7-3: malformed quoting is an error.
    #[test]
    fn r7_3_rejects_malformed_quoting() {
        assert!(PatientIdMapper::from_csv("\"MRN001,ANON-1\n").is_err());
        assert!(PatientIdMapper::from_csv("MRN\"001,ANON-1\n").is_err());
    }

    // -- r-7-4: JSON ---------------------------------------------------------

    /// Requirement r-7-4: an object of pairs.
    #[test]
    fn r7_4_reads_an_object_of_pairs() {
        let mapper = PatientIdMapper::from_json(r#"{"MRN001": "ANON-1", "MRN002": "ANON-2"}"#)
            .expect("parses");
        assert_eq!(mapper.len(), 2);
        assert_eq!(mapper.lookup("MRN001"), Some("ANON-1"));
        assert_eq!(mapper.lookup("MRN002"), Some("ANON-2"));
    }

    /// Requirement r-7-4: an array of [original, replacement] pairs.
    #[test]
    fn r7_4_reads_an_array_of_pairs() {
        let mapper = PatientIdMapper::from_json(r#"[["MRN001", "ANON-1"]]"#).expect("parses");
        assert_eq!(mapper.lookup("MRN001"), Some("ANON-1"));
    }

    /// Requirement r-7-4: an array of objects keyed by column heading.
    #[test]
    fn r7_4_reads_an_array_of_objects() {
        let mapper = PatientIdMapper::from_json(
            r#"[{"PatientID": "MRN001", "DeidPatientID": "ANON-1"},
                {"patient_id": "MRN002", "new": "ANON-2"}]"#,
        )
        .expect("parses");
        assert_eq!(mapper.lookup("MRN001"), Some("ANON-1"));
        assert_eq!(mapper.lookup("MRN002"), Some("ANON-2"));
    }

    /// Requirement r-7-4: a numeric identifier keeps its source text
    /// rather than being reformatted through a float.
    #[test]
    fn r7_4_numbers_keep_their_source_text() {
        let mapper = PatientIdMapper::from_json(r#"{"900719925474099123": 42}"#).expect("parses");
        assert_eq!(mapper.lookup("900719925474099123"), Some("42"));
    }

    /// Requirement r-7-4: a top-level object is always the map itself,
    /// even when its keys resemble column headings.
    #[test]
    fn r7_4_top_level_object_is_always_the_map() {
        let mapper = PatientIdMapper::from_json(r#"{"PatientID": "ANON-1"}"#).expect("parses");
        assert_eq!(
            mapper.lookup("PatientID"),
            Some("ANON-1"),
            "the key must be read as an identifier, not as a heading"
        );
    }

    /// Requirement r-7-4: composite and null values are errors.
    #[test]
    fn r7_4_rejects_non_scalar_values() {
        for bad in [
            r#"{"MRN001": null}"#,
            r#"{"MRN001": ["ANON-1"]}"#,
            r#"{"MRN001": {"a": "b"}}"#,
            r#"{"MRN001": true}"#,
            r#""just a string""#,
            r#"[["MRN001"]]"#,
            r#"[["MRN001", "ANON-1", "extra"]]"#,
            r#"[{"nothing": "useful"}]"#,
            r#"[{"PatientID": "MRN001"}]"#,
            "[1, 2]",
        ] {
            assert!(
                PatientIdMapper::from_json(bad).is_err(),
                "should reject {}",
                bad
            );
        }
    }

    /// Requirement r-7-4: invalid JSON is reported, not partially read.
    #[test]
    fn r7_4_rejects_invalid_json() {
        let err = PatientIdMapper::from_json(r#"{"MRN001": }"#).expect_err("should reject");
        assert!(
            err.to_string().contains("not valid JSON"),
            "unexpected error: {}",
            err
        );
    }

    // -- r-7-5: validation ---------------------------------------------------

    /// Requirement r-7-5: an empty mapper cannot de-identify anything,
    /// so it is refused rather than skipping every file in the run.
    #[test]
    fn r7_5_rejects_a_mapper_with_no_pairs() {
        for empty in ["", "\n\n", "   "] {
            assert!(
                PatientIdMapper::from_csv(empty).is_err(),
                "should reject empty CSV {:?}",
                empty
            );
        }
        assert!(PatientIdMapper::from_json("{}").is_err());
        assert!(PatientIdMapper::from_json("[]").is_err());
    }

    /// Requirement r-7-5: a repeated pair is fine, a conflicting one is
    /// not — otherwise output would depend on row order.
    #[test]
    fn r7_5_rejects_conflicting_duplicates_but_allows_identical_ones() {
        let mapper = PatientIdMapper::from_csv("MRN001,ANON-1\nMRN001,ANON-1\n").expect("parses");
        assert_eq!(mapper.len(), 1);

        let err = PatientIdMapper::from_csv("MRN001,ANON-1\nMRN001,ANON-2\n")
            .expect_err("should reject the conflict");
        assert!(
            err.to_string().contains("already mapped"),
            "unexpected error: {}",
            err
        );
    }

    /// Requirement r-7-5: duplicates are caught in JSON too, where the
    /// keys are object member names.
    #[test]
    fn r7_5_rejects_conflicting_duplicates_in_json() {
        assert!(PatientIdMapper::from_json(r#"{"MRN001": "A", "MRN001": "B"}"#).is_err());
        assert!(PatientIdMapper::from_json(r#"{"MRN001": "A", "MRN001": "A"}"#).is_ok());
    }

    /// Requirement r-7-5: an empty original or replacement is rejected.
    #[test]
    fn r7_5_rejects_empty_identifiers() {
        assert!(PatientIdMapper::from_csv(",ANON-1\n").is_err());
        assert!(PatientIdMapper::from_csv("MRN001,\n").is_err());
        assert!(PatientIdMapper::from_csv("MRN001,\"  \"\n").is_err());
    }

    /// Requirement r-7-5: a replacement must fit VR LO.
    #[test]
    fn r7_5_rejects_a_replacement_longer_than_vr_lo_allows() {
        let long = "A".repeat(LO_MAX_BYTES + 1);
        let err =
            PatientIdMapper::from_csv(&format!("MRN001,{}\n", long)).expect_err("should reject");
        assert!(
            err.to_string().contains("VR LO"),
            "unexpected error: {}",
            err
        );

        // The limit itself must still be accepted.
        let at_limit = "A".repeat(LO_MAX_BYTES);
        assert!(PatientIdMapper::from_csv(&format!("MRN001,{}\n", at_limit)).is_ok());
    }

    /// Requirement r-7-5: a replacement must not carry a backslash,
    /// which DICOM reads as a value separator.
    #[test]
    fn r7_5_rejects_a_replacement_containing_a_backslash() {
        let err = PatientIdMapper::from_csv("MRN001,ANON\\1\n").expect_err("should reject");
        assert!(
            err.to_string().contains("value separator"),
            "unexpected error: {}",
            err
        );
    }

    /// Requirement r-7-5: control characters are rejected.
    #[test]
    fn r7_5_rejects_a_replacement_containing_a_control_character() {
        // Built here rather than written as a JSON escape so the
        // rejection comes from the pair validation, not the parser.
        let replacement = format!("ANON{}1", char::from(7u8));
        let err = PatientIdMapper::from_pairs([("MRN001", replacement.as_str())])
            .expect_err("should reject");
        assert!(
            err.to_string().contains("control character"),
            "unexpected error: {}",
            err
        );
    }

    // -- r-7-6: lookup semantics ---------------------------------------------

    /// Requirement r-7-6: DICOM pads values to an even length, so the
    /// padding must not defeat the lookup.
    #[test]
    fn r7_6_lookup_ignores_dicom_padding() {
        let mapper = mapper(&[("MRN001", "ANON-1")]);
        assert_eq!(mapper.lookup("MRN001 "), Some("ANON-1"));
        assert_eq!(mapper.lookup(" MRN001"), Some("ANON-1"));
    }

    /// Requirement r-7-6: matching is otherwise exact — two identifiers
    /// differing in case are two different patients.
    #[test]
    fn r7_6_lookup_is_case_sensitive() {
        let mapper = mapper(&[("MRN001", "ANON-1")]);
        assert_eq!(mapper.lookup("mrn001"), None);
    }

    /// Requirement r-7-6: an unmapped value is an error, never a
    /// pass-through.
    #[test]
    fn r7_6_unmapped_patient_id_is_an_error() {
        let mapper = mapper(&[("MRN001", "ANON-1")]);
        let mut obj = obj_with_patient_id("MRN999");
        let err = mapper.apply(&mut obj).expect_err("should refuse to map");
        assert!(!err.is_fatal(), "the file is skipped, not the run aborted");
        assert!(
            err.to_string().contains("MRN999"),
            "the error should name the unmapped value: {}",
            err
        );
        assert_eq!(
            patient_id(&obj),
            "MRN999",
            "a refused file must not be half-modified"
        );
    }

    /// Requirement r-7-6: a data set with no PatientID at all is
    /// handled the same way as an unmapped one.
    #[test]
    fn r7_6_missing_patient_id_is_an_error() {
        let mapper = mapper(&[("MRN001", "ANON-1")]);
        let mut obj = InMemDicomObject::new_empty();
        let err = mapper.apply(&mut obj).expect_err("should refuse to map");
        assert!(!err.is_fatal());
        assert!(
            err.to_string().contains("no PatientID"),
            "unexpected error: {}",
            err
        );
    }

    /// Requirement r-7-6: an empty PatientID is handled the same way.
    #[test]
    fn r7_6_empty_patient_id_is_an_error() {
        let mapper = mapper(&[("MRN001", "ANON-1")]);
        let mut obj = obj_with_patient_id("");
        let err = mapper.apply(&mut obj).expect_err("should refuse to map");
        assert!(!err.is_fatal());
        assert!(
            err.to_string().contains("empty PatientID"),
            "unexpected error: {}",
            err
        );
    }

    // -- r-7-7: applying the mapping -----------------------------------------

    /// Requirement r-7-7
    #[test]
    fn r7_7_replaces_the_top_level_patient_id() {
        let mapper = mapper(&[("MRN001", "ANON-1")]);
        let mut obj = obj_with_patient_id("MRN001");
        mapper.apply(&mut obj).expect("should map");
        assert_eq!(patient_id(&obj), "ANON-1");
        assert_eq!(
            obj.element(tags::PATIENT_ID).expect("present").vr(),
            VR::LO,
            "PatientID must keep its dictionary VR"
        );
    }

    /// Requirement r-7-7: a padded value in the data set still maps.
    #[test]
    fn r7_7_maps_a_padded_value() {
        let mapper = mapper(&[("MRN001", "ANON-1")]);
        let mut obj = obj_with_patient_id("MRN001 ");
        mapper.apply(&mut obj).expect("should map");
        assert_eq!(patient_id(&obj), "ANON-1");
    }

    /// Requirement r-7-7: PatientID inside a sequence is replaced too,
    /// at any depth.
    #[test]
    fn r7_7_replaces_patient_id_nested_in_sequences() {
        let mapper = mapper(&[("MRN001", "ANON-1"), ("MRN002", "ANON-2")]);

        let inner = obj_with_patient_id("MRN002");
        let mut middle = InMemDicomObject::new_empty();
        put_sequence(&mut middle, tags::REFERENCED_PATIENT_SEQUENCE, vec![inner]);
        let mut obj = obj_with_patient_id("MRN001");
        put_sequence(&mut obj, tags::REFERENCED_STUDY_SEQUENCE, vec![middle]);

        mapper.apply(&mut obj).expect("should map");

        assert_eq!(patient_id(&obj), "ANON-1");
        let middle = &obj
            .element(tags::REFERENCED_STUDY_SEQUENCE)
            .expect("sequence present")
            .items()
            .expect("has items")[0];
        let inner = &middle
            .element(tags::REFERENCED_PATIENT_SEQUENCE)
            .expect("nested sequence present")
            .items()
            .expect("has items")[0];
        assert_eq!(
            patient_id(inner),
            "ANON-1",
            "nested copies take the top-level PatientID's replacement"
        );
    }

    /// Requirement r-7-7: a nested PatientID that does not itself appear
    /// in the mapper must not discard the file. Files routinely carry
    /// copies inside sequences formatted differently from the top-level
    /// value — zero-padded to a fixed width, for instance — and only the
    /// top-level value identifies the patient.
    #[test]
    fn r7_7_unrecognized_nested_patient_id_does_not_fail_the_file() {
        let mapper = mapper(&[("12345", "ANON-1")]);

        let inner = obj_with_patient_id("1234500000");
        let mut obj = obj_with_patient_id("12345");
        put_sequence(&mut obj, tags::REFERENCED_PATIENT_SEQUENCE, vec![inner]);

        mapper.apply(&mut obj).expect("the file must still map");

        assert_eq!(patient_id(&obj), "ANON-1");
        let inner = &obj
            .element(tags::REFERENCED_PATIENT_SEQUENCE)
            .expect("sequence present")
            .items()
            .expect("has items")[0];
        assert_eq!(
            patient_id(inner),
            "ANON-1",
            "the nested copy is overwritten too, so no original identifier survives"
        );
    }

    /// Requirement r-7-7: the mapper must not introduce PatientID where
    /// the data set does not already carry one — sequence items in
    /// particular hold references, not patient records.
    #[test]
    fn r7_7_does_not_add_patient_id_where_it_is_absent() {
        let mapper = mapper(&[("MRN001", "ANON-1")]);

        let mut item = InMemDicomObject::new_empty();
        item.put(DataElement::new(
            tags::REFERENCED_SOP_INSTANCE_UID,
            VR::UI,
            Value::Primitive(PrimitiveValue::from("1.2.3")),
        ));
        let mut obj = obj_with_patient_id("MRN001");
        put_sequence(&mut obj, tags::REFERENCED_IMAGE_SEQUENCE, vec![item]);

        mapper.apply(&mut obj).expect("should map");

        let item = &obj
            .element(tags::REFERENCED_IMAGE_SEQUENCE)
            .expect("sequence present")
            .items()
            .expect("has items")[0];
        assert!(
            item.element(tags::PATIENT_ID).is_err(),
            "no PatientID may be added to a sequence item that had none"
        );
    }

    /// Requirement r-7-2: nothing but PatientID changes.
    #[test]
    fn r7_2_leaves_every_other_element_untouched() {
        let mapper = mapper(&[("MRN001", "ANON-1")]);
        let mut obj = create_test_file_obj();
        put_str(&mut obj, tags::PATIENT_ID, VR::LO, "MRN001");
        put_str(&mut obj, tags::PATIENT_NAME, VR::PN, "Doe^Jane");
        put_str(&mut obj, tags::PATIENT_BIRTH_DATE, VR::DA, "19700101");
        put_str(&mut obj, tags::ACCESSION_NUMBER, VR::SH, "ACC123");
        // A private tag: mapper mode must not strip it (r-7-2).
        put_str(&mut obj, Tag(0x0009, 0x0010), VR::LO, "ACME");

        mapper.apply(&mut obj).expect("should map");

        assert_eq!(patient_id(&obj), "ANON-1");
        let value = |tag| {
            obj.element(tag)
                .unwrap_or_else(|_| panic!("{:?} should still be present", tag))
                .value()
                .to_str()
                .expect("readable")
                .trim()
                .to_string()
        };
        assert_eq!(value(tags::PATIENT_NAME), "Doe^Jane");
        assert_eq!(value(tags::PATIENT_BIRTH_DATE), "19700101");
        assert_eq!(value(tags::ACCESSION_NUMBER), "ACC123");
        assert_eq!(value(Tag(0x0009, 0x0010)), "ACME");
    }

    // -- r-7-1: file type selection ------------------------------------------

    /// Requirement r-7-1
    #[test]
    fn r7_1_loads_by_extension() {
        let tmp = tempfile::TempDir::new().expect("temp dir");

        let csv_path = tmp.path().join("ids.csv");
        fs::write(&csv_path, "MRN001,ANON-1\n").expect("write csv");
        assert_eq!(
            PatientIdMapper::load(&csv_path)
                .expect("should load csv")
                .lookup("MRN001"),
            Some("ANON-1")
        );

        let json_path = tmp.path().join("ids.JSON");
        fs::write(&json_path, r#"{"MRN001": "ANON-1"}"#).expect("write json");
        assert_eq!(
            PatientIdMapper::load(&json_path)
                .expect("extension matching is case-insensitive")
                .lookup("MRN001"),
            Some("ANON-1")
        );
    }

    /// Requirement r-7-1: an unsupported or missing extension is
    /// reported rather than guessed at.
    #[test]
    fn r7_1_rejects_unsupported_extensions() {
        let tmp = tempfile::TempDir::new().expect("temp dir");

        let xlsx_path = tmp.path().join("ids.xlsx");
        fs::write(&xlsx_path, "MRN001,ANON-1\n").expect("write file");
        let err = PatientIdMapper::load(&xlsx_path).expect_err("should reject");
        assert!(
            err.to_string().contains(".csv or .json"),
            "unexpected error: {}",
            err
        );

        let bare_path = tmp.path().join("ids");
        fs::write(&bare_path, "MRN001,ANON-1\n").expect("write file");
        assert!(PatientIdMapper::load(&bare_path).is_err());
    }

    /// Requirement r-7-1: a missing file is reported.
    #[test]
    fn r7_1_reports_a_missing_file() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let err = PatientIdMapper::load(&tmp.path().join("absent.csv"))
            .expect_err("should report the missing file");
        assert!(
            err.to_string().contains("cannot read"),
            "unexpected: {}",
            err
        );
    }

    /// Requirement r-7-1: a parse error names the file it came from.
    #[test]
    fn r7_1_parse_errors_name_the_file() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let path = tmp.path().join("ids.csv");
        fs::write(&path, "MRN001,ANON-1\nMRN002\n").expect("write csv");
        let err = PatientIdMapper::load(&path).expect_err("should reject");
        assert!(
            err.to_string().contains("ids.csv"),
            "unexpected error: {}",
            err
        );
    }

    /// Requirement r-6-2: the library API accepts pairs already in
    /// memory, with the same validation as a file.
    #[test]
    fn r6_2_from_pairs_builds_and_validates_a_mapper() {
        let mapper = PatientIdMapper::from_pairs([("MRN001", "ANON-1")]).expect("should build");
        assert_eq!(mapper.lookup("MRN001"), Some("ANON-1"));
        assert!(!mapper.is_empty());

        assert!(PatientIdMapper::from_pairs([("MRN001", "")]).is_err());
        assert!(
            PatientIdMapper::from_pairs(Vec::<(String, String)>::new()).is_err(),
            "an empty mapper is refused"
        );
    }

    #[test]
    fn heading_columns_falls_back_when_only_one_is_named() {
        // Only the source heading is recognized: the replacement is
        // taken from the next column.
        assert_eq!(
            heading_columns(&["PatientID".into(), "whatever".into()]),
            (0, 1)
        );
        // The source heading sits in the second column with no
        // recognized replacement heading: fall back to the first.
        assert_eq!(
            heading_columns(&["whatever".into(), "PatientID".into()]),
            (1, 0)
        );
    }
}
