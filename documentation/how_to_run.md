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

One condition aborts the whole run instead of skipping a file: a data set left
with no `SOPInstanceUID` (0008,0018), which makes it impossible to de-identify
`(0002,0003)` consistently. You will see
`Error running pipeline: File meta information cannot be de-identified: ...`
and a non-zero exit code. The usual cause is a recipe that removes or blanks
`SOPInstanceUID`; replace it (e.g. `REPLACE SOPInstanceUID func:hashuid`)
instead.

## 3. What a recipe does

A recipe has up to four kinds of sections (see `README.md` for full syntax):

- `%filter blacklist` — files matching these label conditions are excluded from output entirely.
- `%filter allowlist` — files matching these labels are **exempt from every
  blacklist rule**, and nothing else: they are still masked by the graylist and
  still de-identified by `%header`. This is what lets a recipe state a broad
  rejection alongside narrow exceptions to it — reject all ultrasound, except
  these twelve validated scanners. An allowlist match is *not* a licence to emit
  the file untouched; the devices on such a list are usually ones known to carry
  burned-in PHI, admitted on the understanding that the graylist removes it.
- `%filter graylist` — files matching these labels get the listed pixel regions
  masked (for burned-in PHI such as ultrasound banner text). Region syntax:
  `coordinates x,y,xmax,ymax` or `ctpcoordinates x,y,width,height`.
- `%header` — per-tag metadata actions: `ADD`, `REPLACE`, `REMOVE`, `BLANK`,
  `KEEP`, `JITTER`. When several actions target the same tag, precedence is
  `KEEP > ADD > REPLACE > JITTER > REMOVE > BLANK`.

Three behaviors are hard-coded in the tool regardless of recipe content:

- **All private (odd-group) tags are removed automatically** from every file,
  including inside nested sequences.
- Header actions **recurse into sequences**, so a `REMOVE PersonName`-style
  rule also applies to matching tags nested in sequence items.
- **The File Meta Information group (group 0002) is de-identified and kept
  consistent with the data set.** `(0002,0003)` always ends up equal to the
  de-identified `(0008,0018)`, AE titles and meta private information are
  dropped, and the transfer syntax is preserved. Recipes cannot address group
  0002 tags. See README.md for the full table.

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


## 5. The CTP profile: `ctp_filter.txt` then `ctp_default.txt`

The CTP-derived rules ship as **two** recipes, run one after the other. They are
separate on purpose, so that header de-identification can be run on its own.

| Recipe | Sections | What it decides |
|--------|----------|-----------------|
| `ctp_filter.txt` | `allowlist`, `blacklist`, `graylist` | Which files are admitted at all, and where to mask burned-in PHI |
| `ctp_default.txt` | `header` | What to do to every tag |

`ctp_filter.txt` is generated by merging two CTP scripts that are designed to be
used together, and it is not meant to be edited by hand:

- **`ctp_stanford.script`** — a CTP `DicomFilter` script: pure admission control.
  It is shaped `( device whitelist ) + !( rejection gauntlet )`, meaning *accept
  a file if it is a known-good device, or if the gauntlet does not catch it*.
  Converted to the equivalent rejection rule as an `allowlist` plus a `blacklist`.
- **`ctp_pixel.txt`** — the CTP burned-in annotation library, appended unchanged
  as the `graylist`.

The two halves are inseparable. The whitelist deliberately admits devices that
carry burned-in PHI — the source script annotates them `-- SCRUBBED` — relying
on the graylist to mask it. Running the allowlist without the graylist would
publish that PHI; running the graylist without the blacklist would let every
unlisted ultrasound, secondary capture and scanned document through unmasked.

### Running both stages

```sh
# Stage 1: admission control + pixel masking
./target/release/dicom-deid-rs ./input ./staging ctp_filter.txt

# inspect what was rejected before continuing (see below), then:

# Stage 2: header de-identification
./target/release/dicom-deid-rs ./staging ./output ctp_default.txt \
  --var DATEINC -3210 --salt "$PROJECT_SALT"
```

> **`./staging` is PHI.** It holds masked pixels with *fully identified*
> headers. Delete it when the run is done, and keep it off any share the output
> directory is on.

Between the stages, read the rejection list. Files excluded by the blacklist are
written to `blacklisted_files.txt` **in the current working directory** — not in
the output directory, which is what keeps that directory free of PHI. Each line
names an input path and the rule that matched it, so a large or surprising count
usually means a scanner of yours is missing from the whitelist rather than
anything being wrong with the data.

### Running header de-identification on its own

The stages are independent, so a header-only pass is just stage 2 pointed at the
original input:

```sh
./target/release/dicom-deid-rs ./input ./output ctp_default.txt \
  --var DATEINC -3210 --salt "$PROJECT_SALT"
```

Nothing is rejected and no pixels are touched. This matters most for **MR**,
which the gauntlet treats more harshly than other modalities: plain
`ORIGINAL\PRIMARY` MR passes the filter untouched, and so does `DERIVED\PRIMARY`
MR, but other derived MR — MPRs, subtractions, anything `DERIVED\SECONDARY` or
carrying `MRSC` — is rejected, as is anything with `BurnedInAnnotation` set to
`YES`. If you want those series in your output, run stage 2 alone.

### Adopting it for your own devices

This whitelist is another institution's device inventory, and the filter is
fail-closed: a scanner absent from it is rejected rather than admitted. Expect
rejections until your own devices are added. Run stage 1 over a representative
sample first and read `blacklisted_files.txt` to size that up before committing
to it.

To add a device, add it to `ctp_stanford.script` in the same form as the
existing entries, then regenerate and re-verify:

```sh
tools/ctp_filter_to_recipe.py ctp_stanford.script \
    --graylist-from ctp_pixel.txt --output ctp_filter.txt
tools/ctp_filter_diff_vectors.py ctp_stanford.script \
    --output tests/fixtures/ctp_filter_vectors.tsv
cargo test --test ctp_filter_recipe --test ctp_filter_differential --test ctp_filter_golden
```

The differential test replays ~39,000 synthetic headers through the generated
recipe and compares each decision against an independent reading of the CTP
script, so it will tell you if a change to either file altered behavior in a way
you did not intend. `tools/mutation_check.sh` confirms that test still
discriminates, by breaking the recipe in seven ways a bad conversion would and
checking each is caught.

## 6. Verify a run

1. Confirm `Files skipped: 0` (investigate any warnings otherwise).
2. Spot-check an output file, e.g. with pydicom:

   ```python
   import pydicom
   ds = pydicom.dcmread("output/some/file.dcm")
   print(ds)   # confirm PatientName is hashed, dates gone, kept tags intact
   ```

3. Grep the dump for known PHI from the source data (patient names, MRNs,
   dates of birth) to confirm nothing leaked, including inside sequences.
