//! Structural validation of the generated `ctp_filter.txt`.
//!
//! `ctp_filter.txt` is machine-generated from two CTP scripts by
//! `tools/ctp_filter_to_recipe.py`. Generated or not, it is the file that
//! decides which studies reach researchers, so the properties that would let it
//! fail *silently* are asserted here:
//!
//! - a field name the DICOM dictionary does not know never matches, which
//!   would disable a rule without any error;
//! - a `contains` value that fails to compile as a regex falls back to a
//!   literal substring search for the pattern text, which also never matches.
//!
//! Both are invisible at runtime. Neither is invisible here.

use dicom_core::dictionary::DataDictionary;
use dicom_dictionary_std::StandardDataDictionary;
use regex::Regex;

use dicom_deid_rs::recipe::{FilterType, Predicate, Recipe};

const RECIPE_PATH: &str = "ctp_pixel_deid.txt";

fn load() -> Recipe {
    let text = std::fs::read_to_string(RECIPE_PATH).unwrap_or_else(|e| {
        panic!(
            "{RECIPE_PATH} not readable ({e}). Regenerate with: \
             tools/ctp_filter_to_recipe.py ctp_stanford.script \
             --graylist-from ctp_pixel.txt --output ctp_filter.txt"
        )
    });
    Recipe::parse(&text).expect("generated recipe must parse")
}

/// Every (field, value) pair a predicate carries, with whether it is a regex.
fn predicates(recipe: &Recipe) -> Vec<(FilterType, String, &Predicate)> {
    let mut out = Vec::new();
    for section in &recipe.filters {
        for label in &section.labels {
            for condition in &label.conditions {
                out.push((
                    section.filter_type,
                    label.name.clone(),
                    &condition.predicate,
                ));
            }
        }
    }
    out
}

fn field_of(predicate: &Predicate) -> &str {
    match predicate {
        Predicate::Contains { field, .. }
        | Predicate::NotContains { field, .. }
        | Predicate::Equals { field, .. }
        | Predicate::NotEquals { field, .. }
        | Predicate::Missing { field }
        | Predicate::Empty { field }
        | Predicate::Present { field }
        | Predicate::Blank { field }
        | Predicate::NotBlank { field } => field,
    }
}

/// The generated recipe must carry all three filter sections, and the counts
/// must match what the converter reports. A silent drop of the allowlist would
/// turn the gauntlet into a blanket rejection of most modalities.
#[test]
fn generated_recipe_has_the_expected_sections_and_label_counts() {
    let recipe = load();

    let count = |wanted: FilterType| {
        recipe
            .filters
            .iter()
            .filter(|s| s.filter_type == wanted)
            .map(|s| s.labels.len())
            .sum::<usize>()
    };

    // Cross-check the parsed sections against the counts the generator recorded
    // in the file's own header, rather than against numbers hardcoded here.
    // That catches a truncated or partially written file without turning every
    // legitimate edit to ctp_stanford.script into a test failure.
    let text = std::fs::read_to_string(RECIPE_PATH).expect("recipe readable");
    let stated = |what: &str| -> usize {
        let marker = format!("%filter {what} (");
        let start = text
            .find(&marker)
            .unwrap_or_else(|| panic!("the generated header must record the {what} label count"))
            + marker.len();
        text[start..]
            .split(' ')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("could not read the {what} count from the header"))
    };

    assert_eq!(
        count(FilterType::Allowlist),
        stated("allowlist"),
        "allowlist labels present vs. recorded in the file header"
    );
    assert_eq!(
        count(FilterType::Blacklist),
        stated("blacklist"),
        "blacklist labels present vs. recorded in the file header"
    );
    assert_eq!(
        count(FilterType::Graylist),
        504,
        "graylist labels, copied unchanged from ctp_pixel.txt"
    );
    assert!(
        count(FilterType::Allowlist) > 1000,
        "the allowlist should hold four figures of device rules; found {}",
        count(FilterType::Allowlist)
    );
    assert!(
        recipe.header.is_empty(),
        "the filter recipe must carry no header actions: header de-identification \
         is a separate stage, so that it can be run on its own"
    );
}

/// Every field must resolve to a real DICOM tag. An unresolvable keyword never
/// matches, so a typo here disables a rule with no error at runtime.
#[test]
fn every_field_resolves_to_a_dicom_keyword() {
    let recipe = load();
    let dict = StandardDataDictionary;

    let mut unresolved: Vec<String> = Vec::new();
    for (_, label, predicate) in predicates(&recipe) {
        // A `::` qualified field names a sequence and an element inside it
        // (r-2-6-11); every part must resolve.
        for part in field_of(predicate).split("::") {
            if dict.by_name(part).is_none() {
                unresolved.push(format!("{part} (in label {label:?})"));
            }
        }
    }
    unresolved.sort();
    unresolved.dedup();
    assert!(
        unresolved.is_empty(),
        "these fields do not resolve to DICOM keywords and would never match:\n  {}",
        unresolved.join("\n  ")
    );
}

/// Every `contains`/`notcontains` value must compile as a regex. The evaluator
/// falls back to a literal substring search when compilation fails, so an
/// unescaped metacharacter would make the pattern text itself the search term.
#[test]
fn every_contains_value_compiles_as_a_regex() {
    let recipe = load();

    let mut broken: Vec<String> = Vec::new();
    for (_, label, predicate) in predicates(&recipe) {
        let value = match predicate {
            Predicate::Contains { value, .. } | Predicate::NotContains { value, .. } => value,
            _ => continue,
        };
        // Mirrors how the evaluator compiles it.
        if let Err(e) = Regex::new(&format!("(?i){value}")) {
            broken.push(format!("{value:?} in label {label:?}: {e}"));
        }
    }
    assert!(
        broken.is_empty(),
        "these values do not compile as regexes and would silently never match:\n  {}",
        broken.join("\n  ")
    );
}

/// A regex-escaped literal must still match the text it was escaped from.
/// This is what protects the `ImageType` tests, whose values carry the DICOM
/// multi-value backslash (`DERIVED\PRIMARY`).
#[test]
fn escaped_backslash_values_match_their_literal_text() {
    let recipe = load();

    let mut checked = 0;
    for (_, label, predicate) in predicates(&recipe) {
        let value = match predicate {
            Predicate::Contains { field, value } if field == "ImageType" => value,
            _ => continue,
        };
        if !value.contains("\\\\") {
            continue;
        }
        // Undo the escaping to recover the literal the CTP script asked for,
        // then confirm the pattern actually matches it.
        let literal = value.replace("\\\\", "\\").replace("\\.", ".");
        let literal = literal.trim_start_matches('^');
        let re = Regex::new(&format!("(?i){value}")).expect("must compile");
        assert!(
            re.is_match(literal),
            "pattern {value:?} in label {label:?} does not match the literal \
             {literal:?} it was escaped from"
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "expected at least one backslash-bearing ImageType pattern to check"
    );
}

/// No label name may carry a `#`, which starts a comment in the recipe format,
/// nor a control character. Either would corrupt the parsed name, and label
/// names are what the blacklist report shows the operator.
#[test]
fn label_names_are_clean_and_present() {
    let recipe = load();

    for section in &recipe.filters {
        for label in &section.labels {
            assert!(!label.name.is_empty(), "every label must be named");
            assert!(
                !label.name.contains('#'),
                "label name {:?} contains a comment character",
                label.name
            );
            assert!(
                !label.name.chars().any(|c| c.is_control()),
                "label name {:?} contains a control character",
                label.name
            );
            assert!(
                !label.conditions.is_empty(),
                "label {:?} has no conditions, so it would match everything",
                label.name
            );
        }
    }
}

/// The allowlist and blacklist must carry no coordinate directives: they decide
/// admission, not masking. Masking belongs to the graylist, and every graylist
/// label must have at least one region or it does nothing.
#[test]
fn coordinates_belong_only_to_the_graylist() {
    let recipe = load();

    for section in &recipe.filters {
        for label in &section.labels {
            match section.filter_type {
                FilterType::Graylist => assert!(
                    !label.coordinates.is_empty(),
                    "graylist label {:?} has no regions",
                    label.name
                ),
                _ => assert!(
                    label.coordinates.is_empty(),
                    "{:?} label {:?} carries coordinates, which it cannot act on",
                    section.filter_type,
                    label.name
                ),
            }
        }
    }
}
