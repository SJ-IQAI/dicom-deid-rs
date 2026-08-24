//! Time apply_header_actions against objects of varying sequence shape,
//! to find how cost scales with sequence items and nesting.
//!
//! Usage: cargo run --release --example bench_actions -- <recipe>

use dicom_core::value::{DataSetSequence, PrimitiveValue, Value};
use dicom_core::{DataElement, Length, Tag, VR};
use dicom_deid_rs::functions;
use dicom_deid_rs::metadata;
use dicom_deid_rs::recipe::Recipe;
use dicom_object::InMemDicomObject;
use std::collections::HashMap;
use std::time::Instant;

fn put_str(obj: &mut InMemDicomObject, tag: Tag, vr: VR, value: &str) {
    obj.put(DataElement::new(
        tag,
        vr,
        Value::Primitive(PrimitiveValue::from(value)),
    ));
}

/// A small item with a few common elements.
fn item() -> InMemDicomObject {
    let mut o = InMemDicomObject::new_empty();
    put_str(
        &mut o,
        Tag(0x0008, 0x1150),
        VR::UI,
        "1.2.840.10008.5.1.4.1.1.6.1",
    );
    put_str(&mut o, Tag(0x0008, 0x1155), VR::UI, "1.2.3.4.5.6.7.8");
    o
}

/// Base object: some top-level elements plus one sequence of `n_items`
/// items, each optionally holding a nested sequence of `nested` items.
fn build(top_extra: usize, n_items: usize, nested: usize) -> InMemDicomObject {
    let mut obj = InMemDicomObject::new_empty();
    put_str(&mut obj, Tag(0x0008, 0x0018), VR::UI, "1.2.3.4");
    put_str(&mut obj, Tag(0x0008, 0x0020), VR::DA, "20240101");
    put_str(&mut obj, Tag(0x0010, 0x0020), VR::LO, "MRN1");
    put_str(&mut obj, Tag(0x0010, 0x0010), VR::PN, "Doe^Jane");
    for i in 0..top_extra {
        put_str(&mut obj, Tag(0x0019, (0x1000 + i) as u16), VR::LO, "x");
    }
    if n_items > 0 {
        let items: Vec<InMemDicomObject> = (0..n_items)
            .map(|_| {
                let mut it = item();
                if nested > 0 {
                    let inner: Vec<InMemDicomObject> = (0..nested).map(|_| item()).collect();
                    it.put(DataElement::new(
                        Tag(0x0008, 0x1115),
                        VR::SQ,
                        Value::from(DataSetSequence::new(inner, Length::UNDEFINED)),
                    ));
                }
                it
            })
            .collect();
        obj.put(DataElement::new(
            Tag(0x5200, 0x9230),
            VR::SQ,
            Value::from(DataSetSequence::new(items, Length::UNDEFINED)),
        ));
    }
    obj
}

fn main() {
    let recipe_path = std::env::args().nth(1).expect("recipe path");
    let recipe_text = std::fs::read_to_string(recipe_path).expect("read recipe");
    let recipe = Recipe::parse(&recipe_text).expect("parse recipe");
    let funcs = functions::default_functions(None);
    let mut vars = HashMap::new();
    vars.insert("DATEINC".to_string(), "-3210".to_string());

    let shapes: &[(&str, usize, usize, usize)] = &[
        ("flat, 4 elements", 0, 0, 0),
        ("flat, 54 elements", 50, 0, 0),
        ("seq of 10 items", 0, 10, 0),
        ("seq of 100 items", 0, 100, 0),
        ("seq of 1000 items", 0, 1000, 0),
        ("seq of 5000 items", 0, 5000, 0),
        ("seq of 20000 items", 0, 20000, 0),
        ("10 items x 10 nested", 0, 10, 10),
        ("100 items x 100 nested", 0, 100, 100),
    ];

    for (name, top, items, nested) in shapes {
        let mut obj = build(*top, *items, *nested);
        let t = Instant::now();
        metadata::apply_header_actions(&recipe.header, &vars, &funcs, &mut obj)
            .expect("apply actions");
        let elapsed = t.elapsed();

        // Did the recursion actually reach into the sequence items?
        // (0008,1155) has a REPLACE func:hashuid rule; if recursion ran,
        // the first item's value is hashed. Also count elements injected
        // into that item by REPLACE rules for tags it never carried.
        let detail = if *items > 0 {
            let seq = obj.element(Tag(0x5200, 0x9230)).expect("seq present");
            let first = &seq.value().items().expect("items")[0];
            let ref_uid = first
                .element(Tag(0x0008, 0x1155))
                .ok()
                .and_then(|e| e.value().to_str().ok().map(|s| s.to_string()))
                .unwrap_or_default();
            format!(
                "  [item now has {} elements (was 2); nested UID hashed: {}]",
                first.iter().count(),
                ref_uid.starts_with("2.25.")
            )
        } else {
            String::new()
        };
        println!("{:>24}: {:?}{}", name, elapsed, detail);
    }
}
