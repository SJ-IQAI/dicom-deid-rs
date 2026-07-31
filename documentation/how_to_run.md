# Running dicom-deid-rs

Notes on how to run this fork of dicom-deid-rs.

## 1. Build

A Rust toolchain is required ([rustup.rs](https://rustup.rs) if you don't have one).

```sh
cd dicom-deid-rs
cargo build --release
```

The binary lands at `target/release/dicom-deid-rs`. To build without JPEG 2000
decompression support (avoids the OpenJPEG dependency):

```sh
cargo build --release --no-default-features
```

## 2. Run

```sh
dicom-deid-rs <input_dir> <output_dir> <recipe_file> [--salt VALUE] [--var NAME VALUE]...
```

| Argument | Meaning |
|----------|---------|
| `input_dir` | Directory searched **recursively** for `.dcm` files |
| `output_dir` | Where de-identified files are written; the input directory structure is preserved |
| `recipe_file` | The recipe describing filter and header actions |
| `--var NAME VALUE` | Defines a recipe variable referenced as `var:NAME`; repeatable |

With the keep-only recipe (no variables required as shipped):

```sh
./target/release/dicom-deid-rs ./input ./output resources/keep-only-recipe.txt
```

If you switch the recipe's date handling from removal to jitter (see section 4
of the recipe), pass the offset in days:

```sh
./target/release/dicom-deid-rs ./input ./output resources/keep-only-recipe.txt \
  --var DATEINC 30
```

When the run finishes you get a summary:

```
De-identification complete:
  Files processed:  120
  Files blacklisted: 3
  Files skipped:    0
```

- **processed** — de-identified and written to the output directory
- **blacklisted** — matched a `%filter blacklist` rule and were deliberately *not* written
- **skipped** — failed with an error (a warning line is printed for each; a
  common cause is a recipe tag keyword the DICOM dictionary doesn't recognize).
  **A non-zero skipped count means those files were not de-identified or
  copied — always investigate the warnings.**

## 3. What a recipe does

A recipe has up to three kinds of sections (see `README.md` for full syntax):

- `%filter blacklist` — files matching these label conditions are excluded from output entirely.
- `%filter graylist` — files matching these labels get the listed pixel regions
  masked (for burned-in PHI such as ultrasound banner text). Region syntax:
  `coordinates x,y,xmax,ymax` or `ctpcoordinates x,y,width,height`.
- `%header` — per-tag metadata actions: `ADD`, `REPLACE`, `REMOVE`, `BLANK`,
  `KEEP`, `JITTER`. When several actions target the same tag, precedence is
  `KEEP > ADD > REPLACE > JITTER > REMOVE > BLANK`.

Two behaviors are hard-coded in the tool regardless of recipe content:

- **All private (odd-group) tags are removed automatically** from every file,
  including inside nested sequences.
- Header actions **recurse into sequences**, so a `REMOVE PersonName`-style
  rule also applies to matching tags nested in sequence items.

## 4. The keep-only recipe

The goal: preserve only your allowlist of tags and eliminate PHI everywhere else.

### How "keep only" is implemented

The recipe language has **no wildcard** ("remove everything not listed"), so
the recipe combines three mechanisms:

1. `KEEP` rules for every allowlisted tag — `KEEP` has the highest precedence,
   so these survive any other rule.
2. An explicit `REMOVE`/`BLANK`/`REPLACE` enumeration of the PHI tags from the
   canonical CTP reference profile (`resources/recipes.txt`), translated to
   keywords this tool's dictionary resolves.
3. The automatic private-tag removal described above.

**Caveat:** technical tags on neither list (`Rows`, `Columns`,
`BitsAllocated`, `WindowCenter`, transfer-syntax related tags, …) pass through
unchanged. That is necessary for the files to stay readable, but it means this
is a PHI-targeted profile, not a literal whitelist filter. If you need strict
"drop every unlisted tag" behavior, that requires the library API
(`TagSpecifier::Pattern(".*")` with a `Remove` action plus `Keep` actions),
which the recipe-file parser does not expose.


## 5. Verify a run

1. Confirm `Files skipped: 0` (investigate any warnings otherwise).
2. Spot-check an output file, e.g. with pydicom:

   ```python
   import pydicom
   ds = pydicom.dcmread("output/some/file.dcm")
   print(ds)   # confirm PatientName is hashed, dates gone, kept tags intact
   ```

3. Grep the dump for known PHI from the source data (patient names, MRNs,
   dates of birth) to confirm nothing leaked, including inside sequences.
