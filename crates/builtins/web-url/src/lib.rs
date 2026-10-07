// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

pub mod algorithms;
pub mod url;
pub mod url_pattern;
pub mod url_search_params;
pub mod url_search_params_iterator;

use js::gc::scope::Scope;
use js::Object;

pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    url::URL::add_to_global(scope, global);
    url_pattern::URLPattern::add_to_global(scope, global);
    url_search_params::URLSearchParams::add_to_global(scope, global);
    url_search_params_iterator::URLSearchParamsIterator::add_to_global(scope, global);
    url_search_params::install_symbol_iterator(scope);
    js::class::inherit_from_iterator_prototype::<
        url_search_params_iterator::URLSearchParamsIteratorImpl,
    >(scope)
    .expect("setting a class prototype's prototype can only fail due to OOM");
}
