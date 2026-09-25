# Phantom scanning

The scanner excludes type-surface-only imports from runtime compatibility targets. References reached from a runtime entry point or a legacy deep path still require a runtime package.

The classifier keeps declared dependencies, peers, and declared type-package fallbacks in their existing categories. Remaining hard or soft phantom references reachable only from declarations become `TypeOnly`, regardless of whether an `@types` twin exists.

Classification lives in [[crates/nub-phantom-scan/src/classify.rs#classify]]. Scanner changes bump [[crates/nub-cli/src/dynamic_phantom.rs#PHANTOM_SCANNER_VERSION]] so cached scans and warm install state are recomputed.
