// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `mangle_name` and `mangle_resource_name` against `mangled_names.txt`.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

#[test]
fn mangled_names_match_the_table() {
    for line in include_str!("mangled_names.txt").lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let [wit, member, class] = line.split(' ').collect::<Vec<_>>()[..] else {
            panic!("a table line has three names: {line:?}");
        };
        assert_eq!(wit_name_mapping::mangle_name(wit), member, "{wit}");
        assert_eq!(wit_name_mapping::mangle_resource_name(wit), class, "{wit}");
    }
}
