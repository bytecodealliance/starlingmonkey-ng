// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

mod regexp;
pub mod url_pattern;

use js::gc::scope::Scope;
use js::Object;

pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    url_pattern::URLPattern::add_to_global(scope, global);
}
