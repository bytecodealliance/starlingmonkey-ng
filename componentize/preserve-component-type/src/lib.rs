// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Copies of a core module's `component-type*` custom sections, and their
//! restoration.
//!
//! The `component-type*` sections hold the worlds of the module's wit-bindgen
//! bindings. `wasm-tools component new` consumes and removes them, and the
//! component's own type keeps only the interfaces from which the module imports
//! functions. Linking wit-dylib bindings against the core module again needs the
//! complete worlds: a stream intrinsic such as
//! `[stream-read-0]read-via-stream` is resolved through the type of the
//! function it names, which the module doesn't necessarily import.
//!
//! `scripts/build-runtime.sh` runs [`preserve`] on the runtime's core module
//! before `wasm-tools component new`, and `starling-componentize` runs
//! [`restore`] on the core module it takes out of the runtime component.

use std::borrow::Cow;

use anyhow::Context as _;
use wasm_encoder::{CustomSection, RawSection, Section as _};
use wasmparser::{Parser, Payload};

/// The prefix of the sections [`preserve`] adds: each is named this, followed
/// by the name of the section it copies.
pub const PREFIX: &str = "starling:";

/// The prefix of the names of the sections copied by [`preserve`].
const SOURCE_PREFIX: &str = "component-type";

/// Returns `module` with a copy of each of its `component-type*` sections
/// appended, named [`PREFIX`] followed by the original name.
///
/// Returns `None` if `module` already has a section named with [`PREFIX`].
/// Fails if `module` is not a core module, or has no `component-type*`
/// section.
pub fn preserve(module: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    if Parser::is_component(module) || !Parser::is_core_wasm(module) {
        anyhow::bail!("not a core wasm module");
    }
    let mut copies = Vec::new();
    for payload in Parser::new(0).parse_all(module) {
        let Payload::CustomSection(section) = payload.context("parsing the core module")? else {
            continue;
        };
        if section.name().starts_with(PREFIX) {
            return Ok(None);
        }
        if section.name().starts_with(SOURCE_PREFIX) {
            CustomSection {
                name: Cow::Owned(format!("{PREFIX}{}", section.name())),
                data: Cow::Borrowed(section.data()),
            }
            .append_to(&mut copies);
        }
    }
    if copies.is_empty() {
        anyhow::bail!("the core module has no `{SOURCE_PREFIX}` sections");
    }
    let mut preserved = Vec::with_capacity(module.len() + copies.len());
    preserved.extend_from_slice(module);
    preserved.extend_from_slice(&copies);
    Ok(Some(preserved))
}

/// Returns the core module `module` with the [`PREFIX`] removed from the name
/// of each section whose name starts with it, and the number of renamed
/// sections.
pub fn restore(module: &[u8]) -> anyhow::Result<(Vec<u8>, usize)> {
    let mut restored = Vec::with_capacity(module.len());
    restored.extend_from_slice(module.get(..8).context("truncated core module")?);
    let mut renamed = 0;
    for payload in Parser::new(0).parse_all(module) {
        let payload = payload.context("parsing the core module")?;
        if let Payload::CustomSection(section) = &payload {
            if let Some(name) = section.name().strip_prefix(PREFIX) {
                CustomSection {
                    name: Cow::Borrowed(name),
                    data: Cow::Borrowed(section.data()),
                }
                .append_to(&mut restored);
                renamed += 1;
                continue;
            }
        }
        if let Some((id, range)) = payload.as_section() {
            RawSection {
                id,
                data: &module[range.start as usize..range.end as usize],
            }
            .append_to(&mut restored);
        }
    }
    Ok((restored, renamed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The custom sections of `module`, by name and contents.
    fn custom_sections(module: &[u8]) -> Vec<(String, Vec<u8>)> {
        Parser::new(0)
            .parse_all(module)
            .filter_map(|payload| match payload.unwrap() {
                Payload::CustomSection(section) => {
                    Some((section.name().to_string(), section.data().to_vec()))
                }
                _ => None,
            })
            .collect()
    }

    fn module_with(sections: &[(&str, &[u8])]) -> Vec<u8> {
        let mut module = wasm_encoder::Module::new();
        for (name, data) in sections {
            module.section(&CustomSection {
                name: Cow::Borrowed(name),
                data: Cow::Borrowed(data),
            });
        }
        module.finish()
    }

    #[test]
    fn copies_are_added_and_restored() {
        let module = module_with(&[
            ("component-type:a", b"a"),
            ("name", b"n"),
            ("component-type:b", b"b"),
        ]);
        let preserved = preserve(&module).unwrap().unwrap();
        assert_eq!(&preserved[..module.len()], &module[..]);
        assert_eq!(
            custom_sections(&preserved)[3..],
            [
                ("starling:component-type:a".to_string(), b"a".to_vec()),
                ("starling:component-type:b".to_string(), b"b".to_vec()),
            ]
        );
        assert_eq!(preserve(&preserved).unwrap(), None);

        // `wasm-tools component new` removes the originals.
        let componentized = module_with(&[
            ("name", b"n"),
            ("starling:component-type:a", b"a"),
            ("starling:component-type:b", b"b"),
        ]);
        let (restored, renamed) = restore(&componentized).unwrap();
        assert_eq!(renamed, 2);
        assert_eq!(
            custom_sections(&restored),
            [
                ("name".to_string(), b"n".to_vec()),
                ("component-type:a".to_string(), b"a".to_vec()),
                ("component-type:b".to_string(), b"b".to_vec()),
            ]
        );
    }

    #[test]
    fn modules_without_component_type_sections_are_refused() {
        let error = preserve(&module_with(&[("name", b"n")])).unwrap_err();
        assert!(error.to_string().contains("no `component-type`"), "{error}");
        let error = preserve(&wasm_encoder::Component::new().finish()).unwrap_err();
        assert!(error.to_string().contains("not a core"), "{error}");
    }
}
