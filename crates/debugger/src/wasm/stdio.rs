// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The stdio transport on p3. Messages arrive on stdin and leave on stdout.

use super::pump::PumpConnection;

/// Connect over stdin and stdout.
pub(crate) fn connect() -> PumpConnection {
    let (stdin, read) = wasip3::cli::stdin::read_via_stream();
    let (tx, rx) = wasip3::wit_stream::new::<u8>();
    let written = wasip3::cli::stdout::write_via_stream(rx);
    PumpConnection::new(
        stdin,
        tx,
        async move {
            let _ = written.await;
        },
        read,
    )
}
