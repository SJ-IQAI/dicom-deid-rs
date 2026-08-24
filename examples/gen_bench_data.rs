//! Generate synthetic DICOM files for benchmarking.
//!
//! Usage: cargo run --release --example gen_bench_data -- <out_dir> <count> [rows] [cols] [frames]
//!
//! With a frames argument > 0, files are written as encapsulated
//! multiframe (JPEG Baseline transfer syntax, one fragment per frame)
//! to mimic compressed ultrasound cine loops. The fragment bytes are
//! not real JPEG — nothing in the header-only pipeline decodes them.

use dicom_core::value::{PixelFragmentSequence, PrimitiveValue, Value};
use dicom_core::{DataElement, Tag, VR};
use dicom_object::meta::FileMetaTableBuilder;
use dicom_object::{FileDicomObject, InMemDicomObject};
use std::env;
use std::path::PathBuf;

fn put_str(obj: &mut InMemDicomObject, tag: Tag, vr: VR, value: &str) {
    obj.put(DataElement::new(
        tag,
        vr,
        Value::Primitive(PrimitiveValue::from(value)),
    ));
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let out_dir = PathBuf::from(args.first().expect("out_dir"));
    let count: usize = args.get(1).expect("count").parse().expect("count");
    let rows: u16 = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(512);
    let cols: u16 = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(512);
    let frames: usize = args.get(4).map(|s| s.parse().unwrap_or(0)).unwrap_or(0);

    std::fs::create_dir_all(&out_dir).expect("create out dir");

    let pixel_bytes = vec![0x42u8; rows as usize * cols as usize * 2];
    // frames == 0: native pixel data, Explicit VR LE
    // frames >  0: encapsulated, JPEG Baseline, one fragment per frame
    // frames == -1 (pass "implicit"): native pixel data, Implicit VR LE
    let implicit = args.get(4).is_some_and(|s| s == "implicit");
    let transfer_syntax = if implicit {
        "1.2.840.10008.1.2"
    } else if frames > 0 {
        "1.2.840.10008.1.2.4.50" // JPEG Baseline
    } else {
        "1.2.840.10008.1.2.1"
    };

    for i in 0..count {
        let mut obj = FileDicomObject::new_empty_with_meta(
            FileMetaTableBuilder::new()
                .transfer_syntax(transfer_syntax)
                .media_storage_sop_class_uid("1.2.840.10008.5.1.4.1.1.2")
                .media_storage_sop_instance_uid(format!("1.2.3.4.{}", i))
                .implementation_class_uid("1.2.3.4")
                .build()
                .expect("valid meta"),
        );
        use dicom_dictionary_std::tags;
        put_str(
            &mut obj,
            tags::SOP_CLASS_UID,
            VR::UI,
            "1.2.840.10008.5.1.4.1.1.2",
        );
        put_str(
            &mut obj,
            tags::SOP_INSTANCE_UID,
            VR::UI,
            &format!("1.2.3.4.{}", i),
        );
        put_str(&mut obj, tags::PATIENT_ID, VR::LO, &format!("MRN{:07}", i));
        put_str(&mut obj, tags::PATIENT_NAME, VR::PN, "Doe^Jane");
        put_str(&mut obj, tags::PATIENT_BIRTH_DATE, VR::DA, "19700101");
        put_str(&mut obj, tags::STUDY_DATE, VR::DA, "20240101");
        put_str(&mut obj, tags::SERIES_DATE, VR::DA, "20240101");
        put_str(
            &mut obj,
            tags::ACCESSION_NUMBER,
            VR::SH,
            &format!("ACC{}", i),
        );
        put_str(
            &mut obj,
            tags::STUDY_INSTANCE_UID,
            VR::UI,
            &format!("1.2.3.{}", i / 100),
        );
        put_str(
            &mut obj,
            tags::SERIES_INSTANCE_UID,
            VR::UI,
            &format!("1.2.3.{}.{}", i / 100, i / 10),
        );
        put_str(&mut obj, tags::MODALITY, VR::CS, "CT");
        put_str(&mut obj, tags::MANUFACTURER, VR::LO, "ACME");
        put_str(&mut obj, tags::INSTITUTION_NAME, VR::LO, "General Hospital");
        put_str(
            &mut obj,
            tags::REFERRING_PHYSICIAN_NAME,
            VR::PN,
            "Smith^John",
        );
        put_str(&mut obj, tags::STATION_NAME, VR::SH, "CT01");
        obj.put(DataElement::new(
            tags::ROWS,
            VR::US,
            Value::Primitive(PrimitiveValue::from(rows)),
        ));
        obj.put(DataElement::new(
            tags::COLUMNS,
            VR::US,
            Value::Primitive(PrimitiveValue::from(cols)),
        ));
        obj.put(DataElement::new(
            tags::BITS_ALLOCATED,
            VR::US,
            Value::Primitive(PrimitiveValue::from(16u16)),
        ));
        // A private tag the pipeline will strip.
        put_str(&mut obj, Tag(0x0009, 0x0010), VR::LO, "ACME PRIVATE");
        if frames > 0 {
            put_str(
                &mut obj,
                tags::NUMBER_OF_FRAMES,
                VR::IS,
                &frames.to_string(),
            );
            // One fragment per frame, sized so the whole element matches
            // a real compressed cine: total ~= rows*cols*2 bytes.
            let frag_size = pixel_bytes.len() / frames;
            let fragments: Vec<Vec<u8>> = (0..frames).map(|_| vec![0x42u8; frag_size]).collect();
            obj.put(DataElement::new(
                tags::PIXEL_DATA,
                VR::OB,
                Value::PixelSequence(PixelFragmentSequence::new(Vec::new(), fragments)),
            ));
        } else {
            obj.put(DataElement::new(
                tags::PIXEL_DATA,
                VR::OW,
                Value::Primitive(PrimitiveValue::from(pixel_bytes.clone())),
            ));
        }

        obj.write_to_file(out_dir.join(format!("img{:05}.dcm", i)))
            .expect("write file");
    }
    eprintln!(
        "wrote {} files of {}x{} ({} KB pixel data each) to {}",
        count,
        rows,
        cols,
        pixel_bytes.len() / 1024,
        out_dir.display()
    );
}
