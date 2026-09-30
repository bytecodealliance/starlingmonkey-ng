// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `same_interface` against `interface_versions.txt`, the table the
//! componentizer's copy is tested against too.

#[test]
fn interface_versions_match_the_table() {
    for line in include_str!("interface_versions.txt").lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let [a, b, expected] = line.split(' ').collect::<Vec<_>>()[..] else {
            panic!("a table line has three fields: {line:?}");
        };
        let same = component_model::naming::same_interface(a, b);
        assert_eq!(same, expected == "same", "{a} {b}");
        assert_eq!(
            component_model::naming::same_interface(b, a),
            same,
            "{b} {a}"
        );
    }
}
