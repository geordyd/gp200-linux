# GP-200 interoperability metadata

The public catalog retains model names/IDs, parameter IDs, ranges, choices,
defaults and source hashes needed to interpret GP-200 presets. Vendor-written
effect descriptions, description XML and artwork are omitted. Original editor
programs and research inputs are not distributed with this repository.

Two explicit metadata views are embedded:

- `gp200.json`: editor 1.8.1 metadata, 322 model-slot definitions and 1,400 controls.
- `gp200-editor-1.6.0.json`: 308 model-slot definitions and 1,329 controls.

These describe the editor's controls. They do not contain pedal DSP algorithms,
prove support for every model on every firmware, or establish GP-200LT/GP-200JR
compatibility. Unknown model codes and opaque preset fields remain preserved.

The catalog rejects duplicate model/parameter IDs and invalid bounds. Selecting a
metadata view never writes to a pedal or silently applies defaults. Local default
previews preserve undeclared bytes; device operations use their separate review
and transport paths.

`firmware-metadata-evidence.json` records hashes and factual differences between
these metadata sets. Historical source locators are descriptive provenance;
there is no dependency on the private files named by those locators. The original
algorithm XML hashes are `a874128d894fb2826616413419e2b4701dbb11c5e779fecf2f0ee97db2bfc8cb`
(1.6.0) and `d95d6e019d602e4dafd5b2b0aec7c1494dd52546b0e67f0ee6663d8fa6cb8e0e`
(1.8.1).

New descriptive text must be project-authored or have explicit redistribution
permission. Product/model names identify interoperable hardware and do not
imply manufacturer endorsement.
