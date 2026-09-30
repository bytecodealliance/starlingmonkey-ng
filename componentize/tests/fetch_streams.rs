// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `stream<u8>` values and `fetch`: the world of `fixtures/fetch_streams.{wit,js}`, whose exports
//! hand a `stream<u8>` argument to `fetch` as a request body and return a response's body as a
//! `stream<u8>`. [`shared::UpstreamHooks`] returns the responses to the outgoing requests.

mod shared;

use {
    shared::{chunked_input, read_bytes, HttpCtx as Ctx},
    tokio::sync::OnceCell,
    wasmtime::{Engine, Store},
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
        let linker = shared::http_linker(&ENGINE, &component);
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

/// A response's body piped through an identity `TransformStream` is returned as
/// a `stream<u8>`, and the pipe finishes although the host reads the body
/// directly.
#[tokio::test]
async fn piped_response_body_as_stream() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let settled = store
        .run_concurrent(async |store| {
            let fetching = instance.test_fetch_streams_fetching();
            let body = fetching.call_get_piped_body(store).await?;
            let (bytes, ended) = read_bytes(store, body, usize::MAX).await?;
            assert!(ended, "the body stream must end");
            assert_eq!(bytes, SERVED_BODY.as_bytes());
            let mut settled = false;
            for _ in 0..1_000 {
                settled = fetching.call_piped_body_settled(store).await?;
                if settled {
                    break;
                }
                tokio::task::yield_now().await;
            }
            anyhow::Ok(settled)
        })
        .await??;
    assert!(settled, "the pipe must finish");
    Ok(())
}

/// A response body that the host aborts, returned as a `stream<u8>`, ends after
/// the bytes that arrived, and the abort is reported on stderr, since a
/// `stream` can't hold an error.
#[tokio::test]
async fn aborted_response_body_as_stream() -> anyhow::Result<()> {
    let (mut store, stderr) = shared::http_store_capturing_stderr(&ENGINE, SERVED_BODY);
    let instance = pre().await.instantiate_async(&mut store).await?;
    let text = store
        .run_concurrent(async |store| {
            let body = instance
                .test_fetch_streams_fetching()
                .call_get_broken_body(store)
                .await?;
            let (bytes, ended) = read_bytes(store, body, usize::MAX).await?;
            assert!(ended, "the body stream must end");
            assert_eq!(bytes, b"partial ");
            // The report is printed once the host has resolved the body's
            // trailers.
            let mut text = String::new();
            for _ in 0..1_000 {
                text = shared::pipe_text(&stderr);
                if text.contains("ended the stream early") {
                    break;
                }
                tokio::task::yield_now().await;
            }
            anyhow::Ok(text)
        })
        .await??;
    assert!(
        text.contains("ended the stream early") && text.contains("ConnectionTerminated"),
        "stderr should report the aborted body; got: {text}"
    );
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
    // `text()` reads a body, and reading `body` reads a stream.
    let mut activities: Vec<&str> = messages
        .iter()
        .map(|message| {
            assert!(
                message.contains("`test:fetch-streams/fetching#sync-read-messages`"),
                "{message}"
            );
            let (activity, _) = message
                .split_once(" needs an active event loop")
                .unwrap_or_else(|| panic!("{message}"));
            activity
        })
        .collect();
    activities.sort_unstable();
    assert_eq!(activities, ["reading a body", "reading a stream"]);
    Ok(())
}

/// `run(1)` combines a timer turn, a drained `ReadableStream` and a `fetch`
/// through the WASIp3 HTTP client, all under the call's one event loop.
#[tokio::test]
async fn combined_builtins_export_runs() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let result = store
        .run_concurrent(async |store| {
            instance
                .test_fetch_streams_fetching()
                .call_run(store, 1)
                .await
        })
        .await??;
    assert_eq!(
        result,
        format!("timer=1 stream=part-apart-bpart-c fetch={SERVED_BODY}")
    );
    Ok(())
}

/// Two `delayed` calls in flight at once on the same store, with distinct tags,
/// each return their own tag, so the per-call event loops do not mix up their
/// work. The first, with the shorter delay, counts both calls in flight, which a
/// serialized run would not.
#[tokio::test]
async fn concurrent_async_exports_interleave() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let (first, second) = store
        .run_concurrent(async |store| {
            let fetching = instance.test_fetch_streams_fetching();
            futures::future::join(
                fetching.call_delayed(store, "first".to_string(), 20),
                fetching.call_delayed(store, "second".to_string(), 200),
            )
            .await
        })
        .await?;
    assert_eq!(first?, "first:2");
    assert_eq!(second?, "second:1");
    Ok(())
}
