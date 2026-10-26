//! The frozen protocol v1 strings in `scripts/wire-constants.txt` are still in
//! the source: a rename of the crates must leave them alone.

use std::path::Path;

#[test]
fn frozen_wire_strings_are_unchanged() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let list = std::fs::read_to_string(root.join("scripts/wire-constants.txt"))
        .expect("scripts/wire-constants.txt");
    let mut checked = 0;
    for line in list.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (constant, file) = line.split_once('\t').expect("<string>\t<file>");
        let source = std::fs::read_to_string(root.join(file))
            .unwrap_or_else(|err| panic!("{file}: {err}"));
        assert!(source.contains(constant), "{constant} is gone from {file}");
        checked += 1;
    }
    assert!(checked >= 30, "only {checked} constants listed");
}
