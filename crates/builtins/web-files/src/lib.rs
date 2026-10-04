// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The [File API](https://w3c.github.io/FileAPI/)'s `Blob` and `File` interfaces.
//!
//! `FileReader`, `FileList` and blob URLs are not implemented.

pub mod blob;
pub mod file;

use js::gc::scope::Scope;
use js::Object;

pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    blob::Blob::add_to_global(scope, global);
    file::File::add_to_global(scope, global);
}
