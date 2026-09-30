// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Usage: `preserve-component-type <core module>`
//!
//! Rewrites the module in place with the copies `preserve_component_type::preserve` adds. A module
//! that already has them is left unchanged.

use anyhow::Context as _;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let (Some(path), None) = (args.next(), args.next()) else {
        anyhow::bail!("usage: preserve-component-type <core module>");
    };
    let path = std::path::PathBuf::from(path);
    let module = std::fs::read(&path).with_context(|| format!("reading `{}`", path.display()))?;
    match preserve_component_type::preserve(&module)
        .with_context(|| format!("`{}`", path.display()))?
    {
        Some(preserved) => {
            std::fs::write(&path, preserved)
                .with_context(|| format!("writing `{}`", path.display()))?;
            println!("{}: copied the component-type sections", path.display());
        }
        None => println!("{}: already has the copies", path.display()),
    }
    Ok(())
}
