// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://w3c.github.io/FileAPI/#file-section>

use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::gc::scope::Scope;

use crate::blob::{normalize_type, process_blob_parts, Blob, BlobImpl, BlobPart, BlobPropertyBag};

/// <https://w3c.github.io/FileAPI/#dfn-FilePropertyBag>
#[webidl_dictionary(extends = BlobPropertyBag)]
pub struct FilePropertyBag {
    pub parent: BlobPropertyBag,
    pub last_modified: Option<i64>,
}

/// <https://w3c.github.io/FileAPI/#file-section>
#[webidl_interface(extends = Blob)]
pub struct File {
    parent: BlobImpl,
    /// <https://w3c.github.io/FileAPI/#dfn-name>
    #[no_trace]
    name: String,
    /// <https://w3c.github.io/FileAPI/#dfn-lastModified>
    last_modified: i64,
}

#[webidl_methods]
impl File {
    /// <https://w3c.github.io/FileAPI/#dom-file-file>
    #[constructor]
    fn new(
        scope: &Scope<'_>,
        file_bits: Vec<BlobPart<'_>>,
        file_name: String,
        options: Option<&FilePropertyBag>,
    ) -> Result<Self, ExnThrown> {
        // Step 1: Let _bytes_ be the result of `processing blob parts` given _fileBits_ and
        //     _options_.
        let endings = options.and_then(|options| options.parent.endings);
        let bytes = process_blob_parts(file_bits, endings).map_err(|_| {
            js::exception::report_out_of_memory(scope);
            ExnThrown
        })?;
        // Step 2: Let _n_ be the _fileName_ argument to the constructor.
        let n = file_name;
        // Step 3: Process ``FilePropertyBag`` dictionary argument by running the following
        //     substeps:
        // Step 3.1: If the ``type`` member is provided and is not the empty string, let _t_ be
        //     set to the ``type`` dictionary member. If _t_ contains any characters outside the
        //     range U+0020 to U+007E, then set _t_ to the empty string and return from these
        //     substeps.
        // Step 3.2: Convert every character in _t_ to `ASCII lowercase`.
        let t = options
            .and_then(|options| options.parent.r#type.as_deref())
            .map_or_else(String::new, normalize_type);
        // Step 3.3: If the ``lastModified`` member is provided, let _d_ be set to the
        //     ``lastModified`` dictionary member. If it is not provided, set _d_ to the current
        //     date and time represented as the number of milliseconds since the `Unix Epoch`
        //     (which is the equivalent of `Date.now()` [ECMA-262]).
        let d = options
            .and_then(|options| options.last_modified)
            .unwrap_or_else(now_ms);
        // Step 4: Return a new ``File`` object _F_ such that:
        // Step 4.1: _F_ refers to the _bytes_ `byte sequence`.
        // Step 4.2: _F_.``size`` is set to the number of total bytes in _bytes_.
        // Step 4.3: _F_.``name`` is set to _n_.
        // Step 4.4: _F_.``type`` is set to _t_.
        // Step 4.5: _F_.``lastModified`` is set to _d_.
        Ok(FileImpl {
            parent: BlobImpl::with_bytes(bytes, t),
            name: n,
            last_modified: d,
        })
    }

    /// <https://w3c.github.io/FileAPI/#dfn-name>
    #[getter]
    fn name(&self) -> String {
        self.data().name.clone()
    }

    /// <https://w3c.github.io/FileAPI/#dfn-lastModified>
    #[getter]
    fn last_modified(&self) -> i64 {
        self.data().last_modified
    }
}

/// The current time in milliseconds since the Unix Epoch, as `Date.now()` returns it.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}
