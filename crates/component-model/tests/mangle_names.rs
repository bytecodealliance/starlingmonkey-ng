// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `mangle_name` and `mangle_resource_name` against `mangled_names.txt`, the
//! table the componentizer's copies are tested against too.

#[test]
fn mangled_names_match_the_table() {
    for line in include_str!("mangled_names.txt").lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let [wit, member, class] = line.split(' ').collect::<Vec<_>>()[..] else {
            panic!("a table line has three names: {line:?}");
        };
        assert_eq!(component_model::mangle_name(wit), member, "{wit}");
        assert_eq!(
            component_model::value::mangle_resource_name(wit),
            class,
            "{wit}"
        );
    }
}
