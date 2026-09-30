// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `same_interface` against `interface_versions.txt`.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

#[test]
fn interface_versions_match_the_table() {
    for line in include_str!("interface_versions.txt").lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let [a, b, expected] = line.split(' ').collect::<Vec<_>>()[..] else {
            panic!("a table line has three fields: {line:?}");
        };
        let same = wit_name_mapping::same_interface(a, b);
        assert_eq!(same, expected == "same", "{a} {b}");
        assert_eq!(wit_name_mapping::same_interface(b, a), same, "{b} {a}");
    }
}

#[test]
fn interface_versions_order_by_semver() {
    let version = wit_name_mapping::interface_version;
    assert!(version("a:b/c@1.2.3") < version("a:b/c@1.10.0"));
    assert!(version("a:b/c@0.3.0-rc.1") < version("a:b/c@0.3.0"));
    assert_eq!(version("a:b/c@0.3"), None);
    assert_eq!(version("a:b/c"), None);
}

#[test]
fn module_specifiers_name_the_canonical_version() {
    for (name, specifier) in [
        ("wasi:http/types@0.3.0", "wasi:http/types@0.3"),
        ("wasi:io/poll@0.2.12", "wasi:io/poll@0.2"),
        ("a:b/c@1.2.3+build.1", "a:b/c@1"),
        ("a:b/c@0.0.4", "a:b/c@0.0.4"),
        (
            "wasi:http/types@0.3.0-rc.1+build.1",
            "wasi:http/types@0.3.0-rc.1",
        ),
        ("wasi:http/types", "wasi:http/types"),
        ("wasi:http/types@0.3.0.1", "wasi:http/types@0.3.0.1"),
        ("external", "external"),
    ] {
        assert_eq!(wit_name_mapping::module_specifier(name), specifier);
        assert!(
            wit_name_mapping::same_interface(specifier, name),
            "{specifier} {name}"
        );
    }
}
