// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// `stream<u8>` values meeting `fetch`, end to end.
//
// PREREQUISITE: a runtime build. `just build-runtime` writes the runtime
// component `target/wasm32-wasip3/release/starling.wasm`, which the suites
// link by default. `just test-componentize` builds it and runs every suite.
// `shared::runtime` documents the environment variables that select the dynamic
// link mode instead.
//
// This componentizes ONE world (`fetch-streams`, see
// `fixtures/fetch_streams.{wit,js}`) whose exports hand a `stream<u8>` argument
// to `fetch` as a request body and return a response's body as a `stream<u8>`.
// The p3 `wasi:http` host wired below intercepts every outgoing request: it
// responds to a `POST` with the request's own body and to a `GET` with
// [`SERVED_BODY`].
//
// Componentizing a world takes seconds, so the component is built once and shared
// via a `OnceCell` `Pre`, and each test gets a fresh `Store`.

mod shared;

use {
    shared::{chunked_input, read_bytes, HttpCtx as Ctx},
    tokio::sync::OnceCell,
    wasmtime::{component::Linker, Engine, Store},
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/fetch_streams.wit",
    world: "fetch-streams",
    exports: { default: async },
});

/// The body the harness serves for a `GET`.
const SERVED_BODY: &str = "served from the host";

static ENGINE: std::sync::LazyLock<Engine> = std::sync::LazyLock::new(shared::engine);

/// Componentize the world once and hold the instantiation-ready `Pre`.
async fn pre() -> &'static FetchStreamsPre<Ctx> {
    static PRE: OnceCell<Result<FetchStreamsPre<Ctx>, String>> = OnceCell::const_new();
    shared::build_once(&PRE, async {
        let component = shared::compile_fixture(
            &ENGINE,
            include_str!("fixtures/fetch_streams.wit"),
            "fetch-streams",
            include_str!("fixtures/fetch_streams.js"),
        )
        .await;
        let mut linker = Linker::new(&ENGINE);
        componentize::add_wasi(&mut linker).expect("wasi linker");
        wasmtime_wasi_http::p3::add_to_linker(&mut linker).expect("wasi:http p3 linker");
        componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &["wasi:http/"])
            .expect("trap-stub unknown imports");
        FetchStreamsPre::new(linker.instantiate_pre(&component).expect("instantiate_pre"))
            .expect("FetchStreamsPre")
    })
    .await
}

fn store() -> Store<Ctx> {
    shared::http_store(&ENGINE, SERVED_BODY)
}

/// A lifted `stream<u8>` is the body of an outgoing request, which the host
/// echoes back.
#[tokio::test]
async fn stream_as_request_body() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let (body, _) = chunked_input(store, &[b"posted ", b"in ", b"chunks"], false)?;
            let text = instance
                .test_fetch_streams_fetching()
                .call_post_stream(store, body)
                .await?;
            assert_eq!(text, "posted in chunks");
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// A response's body is returned as a `stream<u8>`.
#[tokio::test]
async fn response_body_as_stream() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let body = instance
                .test_fetch_streams_fetching()
                .call_get_body(store)
                .await?;
            let (bytes, ended) = read_bytes(store, body, usize::MAX).await?;
            assert!(ended, "the body stream must end");
            assert_eq!(bytes, SERVED_BODY.as_bytes());
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// A lifted `stream<u8>` goes out as a request body and comes back as the
/// response body, which is returned as a `stream<u8>`.
#[tokio::test]
async fn stream_through_fetch() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let (body, _) = chunked_input(store, &[b"round ", b"trip"], false)?;
            let echoed = instance
                .test_fetch_streams_fetching()
                .call_post_and_stream(store, body)
                .await?;
            let (bytes, ended) = read_bytes(store, echoed, usize::MAX).await?;
            assert!(ended, "the echoed body must end");
            assert_eq!(bytes, b"round trip");
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// Reading a response body in a sync export, with `text()` or through the body's
/// reader, rejects with a `TypeError` naming the export.
#[tokio::test]
async fn body_reads_in_a_sync_export_reject() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let fetching = instance.test_fetch_streams_fetching();
    store
        .run_concurrent(async |store| fetching.call_hold_responses(store).await)
        .await??;
    assert!(fetching
        .call_sync_read_messages(&mut store)
        .await?
        .is_empty());
    let messages = fetching.call_sync_read_messages(&mut store).await?;
    assert_eq!(messages.len(), 2, "{messages:?}");
    for message in &messages {
        assert!(
            message.starts_with("reading a body needs an active event loop")
                && message.contains("`test:fetch-streams/fetching#sync-read-messages`"),
            "{message}"
        );
    }
    Ok(())
}
