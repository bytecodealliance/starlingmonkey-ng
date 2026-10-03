// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// Component Model streams and futures end-to-end.
//
// PREREQUISITE: a runtime build. `just build-runtime` writes the runtime
// component `target/wasm32-wasip3/release/starling.wasm`, which the suites
// link by default. `just test-componentize` builds it and runs every suite.
// `shared::runtime` documents the environment variables that select the dynamic
// link mode instead.
//
// This componentizes ONE world (`streams`, see `fixtures/streams.{wit,js}`) whose
// exports take and return `stream<T>` and `future<T>` values, which the guest
// handles as `ReadableStream`s and `Promise`s. Each test hands the guest
// host-produced streams and futures and reads back what it returns.
//
// The world also imports `host-thing-interface`, a host-owned resource that
// `short_reads_host` streams, so the elements lift to imported-resource wrappers,
// and `transfers`, whose functions take and return streams and futures.
//
// Componentizing a world takes seconds, so the component is built once and shared
// via a `OnceCell` `Pre`, and each test gets a fresh `Store`.

mod shared;

use shared::{call_failed, chunked_input, collect_all, read_bytes, until};

use {
    std::{
        pin::Pin,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        task::{Context, Poll},
    },
    tokio::sync::OnceCell,
    wasmtime::{
        component::{
            Accessor, Destination, FutureConsumer, FutureReader, HasSelf, Linker, Resource, Source,
            StreamConsumer, StreamProducer, StreamReader, StreamResult, VecBuffer,
        },
        AsContextMut, Engine, Store, StoreContextMut,
    },
    wasmtime_wasi::{p2::pipe::MemoryOutputPipe, WasiCtxView, WasiView},
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/streams.wit",
    world: "streams",
    imports: { default: async },
    exports: { default: async },
    with: {
        "test:streams/host-thing-interface.host-thing": ThingString,
    },
});

/// The host `host-thing`, held in the resource table.
pub struct ThingString(String);

use test::streams::failing::Failure;

struct Ctx {
    wasi: shared::Wasi,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        self.wasi.view()
    }
}

impl test::streams::host_thing_interface::Host for Ctx {}

impl test::streams::host_thing_interface::HostHostThing for Ctx {
    async fn new(&mut self, s: String) -> Resource<ThingString> {
        self.wasi.table().push(ThingString(s)).unwrap()
    }
    async fn get(&mut self, self_: Resource<ThingString>) -> String {
        self.wasi.table().get(&self_).unwrap().0.clone()
    }
    async fn get_static(&mut self, v: Resource<ThingString>) -> String {
        self.wasi.table().get(&v).unwrap().0.clone()
    }
    async fn drop(&mut self, rep: Resource<ThingString>) -> wasmtime::Result<()> {
        self.wasi.table().delete(rep)?;
        Ok(())
    }
}

impl test::streams::failing::Host for Ctx {}

impl test::streams::failing::HostWithStore<Ctx> for HasSelf<Ctx> {
    async fn lookup(_accessor: &Accessor<Ctx, Self>, key: String) -> Result<String, Failure> {
        if key == "present" {
            Ok("value".to_string())
        } else {
            Err(Failure::NotFound(key))
        }
    }
}

impl test::streams::transfers::Host for Ctx {}

impl test::streams::transfers::HostWithStore<Ctx> for HasSelf<Ctx> {
    async fn sum(accessor: &Accessor<Ctx, Self>, s: StreamReader<u32>) -> u32 {
        let (values, _) = collect_all(accessor, s).await.expect("reading the stream");
        values.into_iter().sum()
    }

    async fn count(accessor: &Accessor<Ctx, Self>, n: u32) -> StreamReader<u32> {
        accessor
            .with(|store| StreamReader::new(store, (0..n).collect::<Vec<u32>>()))
            .expect("creating the stream")
    }

    async fn later(accessor: &Accessor<Ctx, Self>, v: u32) -> FutureReader<u32> {
        accessor
            .with(|store| FutureReader::new(store, async move { Ok::<_, std::io::Error>(v) }))
            .expect("creating the future")
    }

    async fn plus_one(accessor: &Accessor<Ctx, Self>, f: FutureReader<u32>) -> u32 {
        let slot = Arc::new(Mutex::new(None));
        accessor
            .with(|store| f.pipe(store, OptionConsumer { slot: slot.clone() }))
            .expect("reading the future");
        until(|| slot.lock().unwrap().is_some()).await;
        let value = slot
            .lock()
            .unwrap()
            .take()
            .expect("the future delivered a value");
        value + 1
    }
}

/// A `StreamProducer<u8>` that produces nothing until the read is cancelled,
/// and records when it is dropped.
struct PendingProducer {
    dropped: Arc<AtomicBool>,
}

impl<D> StreamProducer<D> for PendingProducer {
    type Item = u8;
    type Buffer = VecBuffer<u8>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _store: StoreContextMut<'a, D>,
        _destination: Destination<'a, u8, VecBuffer<u8>>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            Poll::Ready(Ok(StreamResult::Cancelled))
        } else {
            Poll::Pending
        }
    }
}

impl Drop for PendingProducer {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// A `StreamConsumer<T>` that collects every value it receives into a shared
/// `Vec<T>`.
struct CollectConsumer<T> {
    sink: Arc<Mutex<Vec<T>>>,
}

impl<D, T> StreamConsumer<D> for CollectConsumer<T>
where
    T: wasmtime::component::Lift + Send + Sync + 'static,
{
    type Item = T;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, T>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let pending = source.remaining(store.as_context_mut());
        let mut buf: Vec<T> = Vec::with_capacity(pending);
        source.read(store.as_context_mut(), &mut buf)?;
        if !buf.is_empty() {
            self.sink.lock().unwrap().extend(buf);
        }
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// A `StreamConsumer<T>` that reads exactly one element per `poll_consume`,
/// collecting them into a shared `Vec<T>`. Reading one at a time forces the
/// guest's writes into a sequence of partial writes: each one delivers only the
/// one element this consumer takes, so wit-bindgen's `AbiBuffer` advances (and
/// `dealloc_lists`-frees) the sent element and re-lifts (and frees, via
/// `elem_lift`) every still-unsent element each round. That exercises the mixed
/// advance-some / re-lift-some element-stack cleanup that a full-drain consumer
/// never reaches.
struct OneAtATimeConsumer<T> {
    sink: Arc<Mutex<Vec<T>>>,
}

impl<D, T> StreamConsumer<D> for OneAtATimeConsumer<T>
where
    T: wasmtime::component::Lift + Send + Sync + 'static,
{
    type Item = T;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, T>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        // A capacity-1 buffer makes `Source::read` take at most one element.
        let mut buf: Vec<T> = Vec::with_capacity(1);
        source.read(store.as_context_mut(), &mut buf)?;
        if let Some(value) = buf.into_iter().next() {
            self.sink.lock().unwrap().push(value);
        }
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// A `FutureConsumer<T>` that stores the single value it receives in a shared
/// slot.
struct OptionConsumer<T> {
    slot: Arc<Mutex<Option<T>>>,
}

impl<D, T> FutureConsumer<D> for OptionConsumer<T>
where
    T: wasmtime::component::Lift + Send + Sync + 'static,
{
    type Item = T;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, T>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<()>> {
        let mut buf: Vec<T> = Vec::with_capacity(1);
        source.read(store.as_context_mut(), &mut buf)?;
        if let Some(value) = buf.into_iter().next() {
            *self.slot.lock().unwrap() = Some(value);
        }
        Poll::Ready(Ok(()))
    }
}

static ENGINE: std::sync::LazyLock<Engine> = std::sync::LazyLock::new(shared::engine);

/// Componentize the streams world once and hold the instantiation-ready `Pre`.
async fn pre() -> &'static StreamsPre<Ctx> {
    static PRE: OnceCell<Result<StreamsPre<Ctx>, String>> = OnceCell::const_new();
    shared::build_once(&PRE, async {
        let component = shared::compile_fixture(
            &ENGINE,
            include_str!("fixtures/streams.wit"),
            "streams",
            include_str!("fixtures/streams.js"),
        )
        .await;

        let mut linker = Linker::new(&ENGINE);
        componentize::add_wasi(&mut linker).expect("wasi linker");
        Streams::add_to_linker::<_, HasSelf<_>>(&mut linker, |ctx| ctx).expect("add_to_linker");
        // The runtime's per-call event loop sleeps on the WASIp3 monotonic clock
        // between turns (a real host import the loop's driver awaits).
        componentize::trap_unsatisfied_imports(
            &ENGINE,
            &component,
            &mut linker,
            &["test:streams/"],
        )
        .expect("trap-stub unknown imports");

        StreamsPre::new(linker.instantiate_pre(&component).expect("instantiate_pre"))
            .expect("StreamsPre")
    })
    .await
}

/// A fresh store for one test case.
fn store() -> Store<Ctx> {
    Store::new(
        &ENGINE,
        Ctx {
            wasi: shared::Wasi::inherit(),
        },
    )
}

/// A fresh store whose stderr goes to the returned pipe.
fn store_capturing_stderr() -> (Store<Ctx>, MemoryOutputPipe) {
    let (wasi, stderr) = shared::wasi_capturing_stderr();
    (Store::new(&ENGINE, Ctx { wasi }), stderr)
}

/// A host-produced `future<T>` delivering `value`.
fn future_of<T>(store: &Accessor<Ctx>, value: T) -> anyhow::Result<FutureReader<T>>
where
    T: wasmtime::component::Lower + wasmtime::component::Lift + Send + Sync + 'static,
{
    Ok(store
        .with(|store| FutureReader::new(store, async move { Ok::<_, std::io::Error>(value) }))?)
}

/// Read a `future<T>` the guest returned.
async fn read_future<T: wasmtime::component::Lift + Send + Sync + 'static>(
    store: &Accessor<Ctx>,
    future: FutureReader<T>,
) -> anyhow::Result<T> {
    let slot = Arc::new(Mutex::new(None));
    store.with(|store| future.pipe(store, OptionConsumer { slot: slot.clone() }))?;
    until(|| slot.lock().unwrap().is_some()).await;
    let value = slot.lock().unwrap().take();
    value.ok_or_else(|| anyhow::anyhow!("the future delivered no value"))
}

/// The guest pipes the incoming `stream<u8>` through a `TransformStream` and
/// returns its readable side.
#[tokio::test]
async fn echo_stream_u8() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let (input, _) =
                chunked_input(store, &[b"Beware the Jubjub bird, ", b"and shun"], false)?;
            let echoed = instance
                .test_streams_echoes()
                .call_echo_stream_u8(store, input)
                .await?;
            let (bytes, ended) = read_bytes(store, echoed, usize::MAX).await?;
            assert!(ended, "the echoed stream must end");
            assert_eq!(bytes, b"Beware the Jubjub bird, and shun");
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// A byte stream several times the pump's 64 KiB batch, in chunks larger than
/// one batch, arrives whole and in order.
#[tokio::test]
async fn large_byte_stream() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let data: Vec<u8> = (0..210_000u32).map(|i| (i % 251) as u8).collect();
    let chunks: Vec<&[u8]> = data.chunks(70_000).collect();
    store
        .run_concurrent(async |store| {
            let (input, _) = chunked_input(store, &chunks, false)?;
            let echoed = instance
                .test_streams_echoes()
                .call_echo_stream_u8(store, input)
                .await?;
            let (bytes, ended) = read_bytes(store, echoed, usize::MAX).await?;
            assert!(ended, "the echoed stream must end");
            assert!(bytes == data, "the echoed bytes must equal the input");
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// A stream of several times the pump's 64-element batch arrives whole and in
/// order.
#[tokio::test]
async fn many_elements() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let expected: Vec<String> = (0..300).map(|i| format!("element {i}")).collect();
            let input = store.with(|store| StreamReader::new(store, expected.clone()))?;
            let echoed = instance
                .test_streams_echoes()
                .call_echo_stream_string(store, input)
                .await?;
            let (received, ended) = collect_all(store, echoed).await?;
            assert!(ended, "the echoed stream must end");
            assert_eq!(expected, received);
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// The guest returns the incoming `stream<u8>` unread, which hands its handle
/// straight back to the host.
#[tokio::test]
async fn pass_stream_u8() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let (input, _) = chunked_input(store, &[b"one", b"two", b"three"], false)?;
            let passed = instance
                .test_streams_echoes()
                .call_pass_stream_u8(store, input)
                .await?;
            let (bytes, ended) = read_bytes(store, passed, usize::MAX).await?;
            assert!(ended, "the passed-through stream must end");
            assert_eq!(bytes, b"onetwothree");
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// The guest awaits the incoming `future<string>` and returns its value.
#[tokio::test]
async fn echo_future_string() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let expected = "Beware the Jubjub bird, and shun\n\tThe frumious Bandersnatch!";
            let input = future_of(store, expected.to_string())?;
            let echoed = instance
                .test_streams_echoes()
                .call_echo_future_string(store, input)
                .await?;
            assert_eq!(read_future(store, echoed).await?, expected);
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// The `future<u32>` counterpart of [`echo_future_string`].
#[tokio::test]
async fn echo_future_u32() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let input = future_of(store, 0xCAFE_F00Du32)?;
            let echoed = instance
                .test_streams_echoes()
                .call_echo_future_u32(store, input)
                .await?;
            assert_eq!(read_future(store, echoed).await?, 0xCAFE_F00D);
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// A `stream<string>` reaches the guest one string per chunk, and the async
/// generator the guest returns yields them back.
#[tokio::test]
async fn echo_stream_string() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let expected: Vec<String> = ["alpha", "", "βeta", "gamma\n\tδ", "ω"]
                .into_iter()
                .map(str::to_string)
                .collect();
            let input = store.with(|store| StreamReader::new(store, expected.clone()))?;
            let echoed = instance
                .test_streams_echoes()
                .call_echo_stream_string(store, input)
                .await?;
            let received = Arc::new(Mutex::new(Vec::new()));
            store.with(|store| {
                echoed.pipe(
                    store,
                    CollectConsumer {
                        sink: received.clone(),
                    },
                )
            })?;
            until(|| received.lock().unwrap().len() >= expected.len()).await;
            assert_eq!(&expected, &*received.lock().unwrap());
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// As [`echo_stream_string`], with the host reading one element per read, which
/// forces the guest's writes into a sequence of partial writes.
#[tokio::test]
async fn short_reads_string() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let expected: Vec<String> = ["alpha", "", "βeta", "gamma\n\tδ", "ω", "last"]
                .into_iter()
                .map(str::to_string)
                .collect();
            let input = store.with(|store| StreamReader::new(store, expected.clone()))?;
            let echoed = instance
                .test_streams_echoes()
                .call_echo_stream_string(store, input)
                .await?;
            let received = Arc::new(Mutex::new(Vec::new()));
            store.with(|store| {
                echoed.pipe(
                    store,
                    OneAtATimeConsumer {
                        sink: received.clone(),
                    },
                )
            })?;
            until(|| received.lock().unwrap().len() >= expected.len()).await;
            assert_eq!(&expected, &*received.lock().unwrap());
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// A `stream<point>` round-trips its records through the guest.
#[tokio::test]
async fn echo_stream_point() -> anyhow::Result<()> {
    use exports::test::streams::echoes::Point;

    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let expected: Vec<Point> = (0..5u32).map(|i| Point { x: i, y: i * 10 }).collect();
            let input = store.with(|store| StreamReader::new(store, expected.clone()))?;
            let echoed = instance
                .test_streams_echoes()
                .call_echo_stream_point(store, input)
                .await?;
            let received = Arc::new(Mutex::new(Vec::new()));
            store.with(|store| {
                echoed.pipe(
                    store,
                    CollectConsumer {
                        sink: received.clone(),
                    },
                )
            })?;
            until(|| received.lock().unwrap().len() >= expected.len()).await;
            let received: Vec<(u32, u32)> = received
                .lock()
                .unwrap()
                .iter()
                .map(|p| (p.x, p.y))
                .collect();
            let expected: Vec<(u32, u32)> = expected.iter().map(|p| (p.x, p.y)).collect();
            assert_eq!(received, expected);
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// A `stream<thing>` of guest-owned resources round-trips through the guest,
/// and each echoed handle still reaches the same `thing`.
#[tokio::test]
async fn echo_stream_thing() -> anyhow::Result<()> {
    use wasmtime::component::ResourceAny;

    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let strings = ["a", "b", "c", "d", "e"];
    let mut things: Vec<ResourceAny> = Vec::with_capacity(strings.len());
    for s in strings {
        let thing = instance
            .test_streams_echoes()
            .thing()
            .call_constructor(&mut store, s)
            .await?;
        things.push(thing);
    }

    let echoed_things = store
        .run_concurrent(async |store| {
            let input = store.with(|store| StreamReader::new(store, things))?;
            let echoed = instance
                .test_streams_echoes()
                .call_echo_stream_thing(store, input)
                .await?;
            let received = Arc::new(Mutex::new(Vec::<ResourceAny>::new()));
            store.with(|store| {
                echoed.pipe(
                    store,
                    CollectConsumer {
                        sink: received.clone(),
                    },
                )
            })?;
            until(|| received.lock().unwrap().len() >= strings.len()).await;
            let received = std::mem::take(&mut *received.lock().unwrap());
            anyhow::Ok(received)
        })
        .await??;

    assert_eq!(echoed_things.len(), strings.len());
    for (got, want) in echoed_things.into_iter().zip(strings) {
        let value = instance
            .test_streams_echoes()
            .thing()
            .call_get(&mut store, got)
            .await?;
        assert_eq!(value, want, "echoed thing must wrap the same string");
        got.resource_drop_async(&mut store).await?;
    }
    Ok(())
}

/// A `stream<u8>` is a readable byte stream, so a BYOB reader reads it into its
/// own buffers, here smaller than the chunks the host delivers.
#[tokio::test]
async fn byob_read() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let (input, _) =
                chunked_input(store, &[b"Twas brillig, and the ", b"slithy toves"], false)?;
            let text = instance
                .test_streams_echoes()
                .call_byob_read(store, input)
                .await?;
            assert_eq!(text, "Twas brillig, and the slithy toves");
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// A returned async generator is converted as `ReadableStream.from` converts it,
/// and the pump keeps the call's event loop alive across the timers the
/// generator waits on between chunks.
#[tokio::test]
async fn async_generator_chunks_across_timers() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let chunks = instance
                .test_streams_echoes()
                .call_timed_chunks(store, 5)
                .await?;
            let (bytes, ended) = read_bytes(store, chunks, usize::MAX).await?;
            assert!(ended, "the stream must end after the last chunk");
            assert_eq!(bytes, [0, 1, 2, 3, 4]);
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// A stream the guest read from is returned through the pump, which delivers
/// what the guest left unread.
#[tokio::test]
async fn partly_read_stream_returns_the_rest() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let (input, _) = chunked_input(store, &[b"first", b"second", b"third"], false)?;
            let rest = instance
                .test_streams_echoes()
                .call_after_first_chunk(store, input)
                .await?;
            let (bytes, ended) = read_bytes(store, rest, usize::MAX).await?;
            assert!(ended, "the rest of the stream must end");
            assert_eq!(bytes, b"secondthird");
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// Cancelling a lifted stream drops its readable end, which the host sees as its
/// producer being dropped, even though the producer never ends by itself.
#[tokio::test]
async fn cancel_after_first_chunk() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let (input, dropped) = chunked_input(store, &[b"0123456789"], true)?;
            let read = instance
                .test_streams_echoes()
                .call_cancel_after_first_chunk(store, input)
                .await?;
            assert_eq!(read, 10, "the guest reads the first chunk");
            until(|| dropped.load(Ordering::SeqCst)).await;
            assert!(
                dropped.load(Ordering::SeqCst),
                "cancelling the stream must drop the host's producer"
            );
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// A host that stops reading a returned stream cancels the guest's source, and
/// the instance then streams again.
#[tokio::test]
async fn host_stops_reading() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let cancelled = store
        .run_concurrent(async |store| {
            let endless = instance.test_streams_echoes().call_endless(store).await?;
            let (bytes, ended) = read_bytes(store, endless, 4096).await?;
            assert!(
                ended && bytes.len() >= 4096,
                "the host reads, then hangs up"
            );
            let mut cancelled = false;
            for _ in 0..1_000 {
                cancelled = instance
                    .test_streams_echoes()
                    .call_endless_cancelled(store)
                    .await?;
                if cancelled {
                    break;
                }
                tokio::task::yield_now().await;
            }
            // The abandoned pump leaves the instance usable for another stream.
            let chunks = instance
                .test_streams_echoes()
                .call_timed_chunks(store, 3)
                .await?;
            let (bytes, ended) = read_bytes(store, chunks, usize::MAX).await?;
            assert!(ended, "the later stream must end");
            assert_eq!(bytes, [0, 1, 2]);
            anyhow::Ok(cancelled)
        })
        .await??;
    assert!(cancelled, "the guest's source must be cancelled");
    Ok(())
}

/// A source producing chunks a `stream<u8>` cannot hold ends the stream early,
/// and the runtime reports why on stderr.
#[tokio::test]
async fn wrong_chunks_end_the_stream() -> anyhow::Result<()> {
    let (mut store, stderr) = store_capturing_stderr();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let stream = instance
                .test_streams_echoes()
                .call_wrong_chunks(store)
                .await?;
            let (bytes, ended) = read_bytes(store, stream, usize::MAX).await?;
            assert!(ended, "the stream must end");
            assert!(bytes.is_empty(), "no chunk can be delivered");
            anyhow::Ok(())
        })
        .await??;
    let stderr = shared::pipe_text(&stderr);
    assert!(
        stderr.contains("a stream<u8> chunk must be an ArrayBufferView or an ArrayBuffer"),
        "stderr must say why the stream ended; got: {stderr}"
    );
    Ok(())
}

/// Returning a value that is neither a `ReadableStream` nor iterable for a
/// `stream<u8>` traps.
#[tokio::test]
async fn not_a_stream_traps() -> anyhow::Result<()> {
    let (mut store, stderr) = store_capturing_stderr();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let outcome = store
        .run_concurrent(async |store| {
            instance
                .test_streams_echoes()
                .call_not_a_stream(store)
                .await
                .map(drop)
        })
        .await;
    assert!(call_failed(&outcome), "returning 42 for a stream must trap");
    let stderr = shared::pipe_text(&stderr);
    assert!(
        stderr.contains("expected a ReadableStream or an iterable object, got 42"),
        "stderr must say why; got: {stderr}"
    );
    Ok(())
}

/// A returned promise is the `future<result<string, string>>` itself: fulfilling
/// it delivers `ok`, and rejecting it with an `Error` delivers its message as
/// `err`, since a `string` can hold it.
#[tokio::test]
async fn future_result() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let echoes = instance.test_streams_echoes();
            let fulfilled = echoes
                .call_future_result(store, true, "fine".to_string())
                .await?;
            assert_eq!(read_future(store, fulfilled).await?, Ok("fine".to_string()));
            let rejected = echoes
                .call_future_result(store, false, "broken".to_string())
                .await?;
            assert_eq!(
                read_future(store, rejected).await?,
                Err("broken".to_string())
            );
            let bare = echoes
                .call_future_result_bare(store, "bare".to_string())
                .await?;
            assert_eq!(read_future(store, bare).await?, Err("bare".to_string()));
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// A lifted `future<result<string, string>>` fulfills with its `ok` payload and
/// rejects with a `ComponentError` holding its `err` payload.
#[tokio::test]
async fn future_result_settles_with_ok_or_component_error() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let echoes = instance.test_streams_echoes();
            let ok = future_of(store, Ok::<String, String>("yes".to_string()))?;
            assert_eq!(
                echoes.call_describe_future_result(store, ok).await?,
                "ok: yes"
            );
            let err = future_of(store, Err::<String, String>("no".to_string()))?;
            assert_eq!(
                echoes.call_describe_future_result(store, err).await?,
                "err: true no"
            );
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// A lifted `future<result<string, failure>>` rejects with an instance of the
/// class of its named `err` type, holding the payload. The exported `echoes`
/// names the type through `use`, so the class is the one `failing` exports.
#[tokio::test]
async fn future_result_rejects_with_the_named_error_class() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let described = store
        .run_concurrent(async |store| {
            let echoes = instance.test_streams_echoes();
            let err = future_of(
                store,
                Err::<String, Failure>(Failure::NotFound("key".to_string())),
            )?;
            anyhow::Ok(echoes.call_describe_failure_future(store, err).await?)
        })
        .await??;
    assert_eq!(
        described, r#"true true Failure {"tag":"notFound","val":"key"}"#,
        "the rejection must be an instance of the `Failure` the guest imports"
    );
    Ok(())
}

/// A `stream<host-thing>` reaches the guest as instances of the imported
/// resource's class, and each one returns to the host intact, read back one per
/// read.
#[tokio::test]
async fn stream_of_host_resources_round_trips() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let strings = ["a", "b", "c", "d", "e"];

    let mut things = Vec::with_capacity(strings.len());
    for s in strings {
        things.push(
            store
                .data_mut()
                .wasi
                .table()
                .push(ThingString(s.to_string()))?,
        );
    }

    let received = store
        .run_concurrent(async |store| {
            let input = store.with(|store| StreamReader::new(store, things))?;
            let echoed = instance
                .test_streams_echoes()
                .call_short_reads_host(store, input)
                .await?;
            let received = Arc::new(Mutex::new(Vec::<Resource<ThingString>>::new()));
            store.with(|store| {
                echoed.pipe(
                    store,
                    OneAtATimeConsumer {
                        sink: received.clone(),
                    },
                )
            })?;
            until(|| received.lock().unwrap().len() >= strings.len()).await;
            let received = std::mem::take(&mut *received.lock().unwrap());
            anyhow::Ok(received)
        })
        .await??;

    let values: Vec<String> = received
        .into_iter()
        .map(|thing| store.data_mut().wasi.table().delete(thing).unwrap().0)
        .collect();
    assert_eq!(values, strings, "every host thing must come back in order");
    Ok(())
}

/// An imported function's `err` arm reaches the guest as an instance of the
/// class its `err` type has, which the interface's module exports and which
/// extends `ComponentError`.
#[tokio::test]
async fn import_errors_are_error_class_instances() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let echoes = instance.test_streams_echoes();
            let found = echoes
                .call_describe_lookup(store, "present".to_string())
                .await?;
            assert_eq!(found, "found value");
            let missing = echoes
                .call_describe_lookup(store, "absent".to_string())
                .await?;
            assert_eq!(
                missing,
                r#"true true Failure {"tag":"notFound","val":"absent"}"#
            );
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// An export rethrowing an imported function's error returns its payload as
/// `err`, and a promise lowered to a `future<result<string, failure>>` that
/// rejects with an `Error` named after a payload-less case of `failure` delivers
/// that case.
#[tokio::test]
async fn error_classes_lower_to_err() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let echoes = instance.test_streams_echoes();
            let forwarded = echoes
                .call_forward_lookup(store, "absent".to_string())
                .await?;
            assert!(
                matches!(&forwarded, Err(Failure::NotFound(key)) if key == "absent"),
                "the rethrown error's payload must come back as err; got {forwarded:?}"
            );
            let denied = echoes.call_future_denied(store).await?;
            let denied = read_future(store, denied).await?;
            assert!(
                matches!(denied, Err(Failure::Denied)),
                "an Error naming `denied` must deliver it; got {denied:?}"
            );
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// The guest passes a stream and a future to imported functions, and reads the
/// stream and the future imported functions return.
#[tokio::test]
async fn import_streams_and_futures() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let described = store
        .run_concurrent(async |store| {
            instance
                .test_streams_echoes()
                .call_use_transfers(store)
                .await
        })
        .await??;
    assert_eq!(described, "sum=10 count=0,1,2 later=7 plus-one=42");
    Ok(())
}

/// A chunk a `stream<point>` cannot hold ends the stream after the chunks
/// before it, and the runtime reports which field failed and why on stderr.
#[tokio::test]
async fn wrong_element_ends_the_stream() -> anyhow::Result<()> {
    let (mut store, stderr) = store_capturing_stderr();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let points = store
        .run_concurrent(async |store| {
            let stream = instance
                .test_streams_echoes()
                .call_wrong_points(store)
                .await?;
            let (points, ended) = collect_all(store, stream).await?;
            assert!(ended, "the stream must end");
            anyhow::Ok(points)
        })
        .await??;
    let points: Vec<(u32, u32)> = points.iter().map(|p| (p.x, p.y)).collect();
    assert_eq!(points, [(1, 2)]);
    let stderr = shared::pipe_text(&stderr);
    assert!(
        stderr.contains("a chunk `.x`: expected a u32") && stderr.contains(r#"got "a""#),
        "stderr must say which field failed; got: {stderr}"
    );
    Ok(())
}

/// A chunk is read once, when the stream takes it, so a getter that returns
/// something else on a later read does not change what is sent.
#[tokio::test]
async fn chunk_is_read_once() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let points = store
        .run_concurrent(async |store| {
            let stream = instance
                .test_streams_echoes()
                .call_shifting_points(store)
                .await?;
            let (points, ended) = collect_all(store, stream).await?;
            assert!(ended, "the stream must end");
            anyhow::Ok(points)
        })
        .await??;
    let points: Vec<(u32, u32)> = points.iter().map(|p| (p.x, p.y)).collect();
    assert_eq!(points, [(1, 2)]);
    Ok(())
}

/// Dropping the reader of a `stream<string>`, a `stream<list<u32>>` or a
/// `future<string>` the guest writes hands the unsent element back to the guest,
/// whose lift then owns its strings and lists. Many rounds of that, each
/// followed by calls that allocate strings, leave the instance working: every
/// call completes, with the right result.
///
/// A guest that loops forever blocks the thread polling it, which no async
/// timeout interrupts, so the calls run on a thread of their own.
#[test]
fn dropped_readers_leave_the_instance_intact() -> anyhow::Result<()> {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("creating a runtime");
        let _ = done.send(runtime.block_on(drop_readers_and_ping()));
    });
    match finished.recv_timeout(std::time::Duration::from_secs(120)) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            anyhow::bail!("the instance stopped completing calls")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            anyhow::bail!("the calls panicked")
        }
    }
}

async fn drop_readers_and_ping() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let echoes = instance.test_streams_echoes();
            for round in 0..250 {
                let mut strings = echoes.call_many_strings(store).await?;
                strings.close_with(store)?;
                let mut lists = echoes.call_many_lists(store).await?;
                lists.close_with(store)?;
                let mut later = echoes.call_string_later(store).await?;
                later.close_with(store)?;
                for call in 0..4 {
                    let s = format!("round {round} call {call} ω ").repeat(20);
                    let expected = format!("{s}{}", s.encode_utf16().count());
                    assert_eq!(echoes.call_ping(store, s).await?, expected);
                }
                if round % 50 == 49 {
                    let expected = vec![format!("round {round}").repeat(10), "ω".repeat(30)];
                    let input = store.with(|store| StreamReader::new(store, expected.clone()))?;
                    let echoed = echoes.call_echo_stream_string(store, input).await?;
                    let (received, ended) = collect_all(store, echoed).await?;
                    assert!(ended, "the echoed stream must end");
                    assert_eq!(expected, received, "round {round}");
                }
            }
            anyhow::Ok(())
        })
        .await?
}

/// A `stream<u8>` takes `ArrayBuffer` chunks as well as views.
#[tokio::test]
async fn array_buffer_chunks() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let stream = instance
                .test_streams_echoes()
                .call_array_buffer_chunks(store)
                .await?;
            let (bytes, ended) = read_bytes(store, stream, usize::MAX).await?;
            assert!(ended, "the stream must end");
            assert_eq!(bytes, [1, 2, 3]);
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// Empty chunks a guest's `stream<u8>` source yields are skipped.
#[tokio::test]
async fn empty_chunks_are_skipped() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let stream = instance
                .test_streams_echoes()
                .call_empty_chunks(store)
                .await?;
            let (bytes, ended) = read_bytes(store, stream, usize::MAX).await?;
            assert!(ended, "the stream must end");
            assert_eq!(bytes, [1, 2, 3]);
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// Returning a `ReadableStream` a reader has locked for a `stream<u8>` traps,
/// since nothing else can read it.
#[tokio::test]
async fn locked_stream_traps() -> anyhow::Result<()> {
    let (mut store, stderr) = store_capturing_stderr();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let outcome = store
        .run_concurrent(async |store| {
            instance
                .test_streams_echoes()
                .call_locked_stream(store)
                .await
                .map(drop)
        })
        .await;
    assert!(call_failed(&outcome), "returning a locked stream must trap");
    let stderr = shared::pipe_text(&stderr);
    assert!(
        stderr.contains("the stream is locked"),
        "stderr must say the stream is locked; got: {stderr}"
    );
    Ok(())
}

/// A plain value the guest hands over as a `future<u32>` is written as the
/// future's value.
#[tokio::test]
async fn plain_value_as_future() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let result = store
        .run_concurrent(async |store| {
            instance
                .test_streams_echoes()
                .call_plus_one_plain(store)
                .await
        })
        .await??;
    assert_eq!(result, 42);
    Ok(())
}

/// A value for an import's `stream<T>` argument that does not lower rejects the
/// call with a `TypeError`, and an iterable object is read as the stream.
#[tokio::test]
async fn stream_argument_misuse_rejects() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let messages = store
        .run_concurrent(async |store| {
            instance
                .test_streams_echoes()
                .call_stream_misuse_messages(store)
                .await
        })
        .await??;
    assert_eq!(messages.len(), 4, "{messages:?}");
    assert_eq!(
        messages[0],
        "argument 1 of `sum`: expected a ReadableStream or an iterable object, got 5"
    );
    assert!(
        messages[1].starts_with(
            "argument 1 of `sum`: expected a ReadableStream or an iterable object, got an object: "
        ),
        "{}",
        messages[1]
    );
    assert_eq!(messages[2], "argument 1 of `sum`: the stream is locked");
    assert_eq!(messages[3], "3");
    Ok(())
}

/// Reading a stream in a sync export rejects with a `TypeError` naming the
/// export.
#[tokio::test]
async fn stream_read_in_a_sync_export_rejects() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let echoes = instance.test_streams_echoes();
    store
        .run_concurrent(async |store| {
            let (input, _) = chunked_input(store, &[b"unread"], false)?;
            echoes.call_hold_stream(store, input).await?;
            anyhow::Ok(())
        })
        .await??;
    assert_eq!(echoes.call_sync_stream_message(&mut store).await?, "none");
    let message = echoes.call_sync_stream_message(&mut store).await?;
    assert!(
        message.starts_with("reading a stream needs an active event loop")
            && message.contains("`test:streams/echoes#sync-stream-message`"),
        "{message}"
    );
    Ok(())
}

/// A promise lowered to a `future<u32>` that rejects traps the instance, since the
/// future has no way to deliver the rejection.
#[tokio::test]
async fn rejected_future_traps() -> anyhow::Result<()> {
    let (mut store, stderr) = store_capturing_stderr();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let outcome = store
        .run_concurrent(async |store| {
            let future = instance
                .test_streams_echoes()
                .call_rejected_future(store)
                .await?;
            read_future(store, future).await
        })
        .await;
    assert!(
        !matches!(outcome, Ok(Ok(_))),
        "a rejected future<u32> must trap"
    );
    let stderr = shared::pipe_text(&stderr);
    assert!(
        stderr.contains("component trapped") && stderr.contains("no value"),
        "stderr must name the rejection; got: {stderr}"
    );
    Ok(())
}

/// A promise lowered to a `future<result<u32, u32>>` that rejects with an `Error`
/// traps the instance, since a `u32` `err` cannot hold it.
#[tokio::test]
async fn rejected_result_future_traps() -> anyhow::Result<()> {
    let (mut store, stderr) = store_capturing_stderr();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let outcome = store
        .run_concurrent(async |store| {
            let future = instance
                .test_streams_echoes()
                .call_rejected_result_future(store)
                .await?;
            read_future(store, future).await
        })
        .await;
    assert!(
        !matches!(outcome, Ok(Ok(_))),
        "an unholdable rejection must trap"
    );
    let stderr = shared::pipe_text(&stderr);
    assert!(
        stderr.contains("its error type") && stderr.contains("no value"),
        "stderr must say the error type cannot hold the rejection; got: {stderr}"
    );
    Ok(())
}

/// A host that drops a future's reader before the guest's promise settles
/// leaves the instance usable: the guest's write fails quietly.
#[tokio::test]
async fn host_drops_future_reader() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let echoes = instance.test_streams_echoes();
            let mut dropped = echoes.call_slow_future(store, 5).await?;
            dropped.close_with(store)?;
            let kept = echoes.call_slow_future(store, 6).await?;
            assert_eq!(read_future(store, kept).await?, 6);
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// A `future<thing>` delivers a guest-owned resource, whose handle reaches the
/// object the guest created.
#[tokio::test]
async fn future_of_a_resource() -> anyhow::Result<()> {
    use wasmtime::component::ResourceAny;

    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let thing = store
        .run_concurrent(async |store| {
            let future = instance
                .test_streams_echoes()
                .call_thing_future(store, "held".to_string())
                .await?;
            anyhow::Ok(read_future::<ResourceAny>(store, future).await?)
        })
        .await??;
    let value = instance
        .test_streams_echoes()
        .thing()
        .call_get(&mut store, thing)
        .await?;
    assert_eq!(value, "held");
    thing.resource_drop_async(&mut store).await?;
    Ok(())
}

/// Cancelling a stream while its read from the host is in flight cancels the
/// read and drops the host's producer.
#[tokio::test]
async fn cancel_with_a_read_in_flight() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let dropped = Arc::new(AtomicBool::new(false));
            let input = store.with(|store| {
                StreamReader::new(
                    store,
                    PendingProducer {
                        dropped: dropped.clone(),
                    },
                )
            })?;
            let outcome = instance
                .test_streams_echoes()
                .call_cancel_pending_read(store, input)
                .await?;
            assert_eq!(outcome, "cancelled");
            until(|| dropped.load(Ordering::SeqCst)).await;
            assert!(
                dropped.load(Ordering::SeqCst),
                "cancelling must drop the host's producer"
            );
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// A received stream returned under another stream type is not handed back by
/// its handle, and the pump delivers its bytes instead.
#[tokio::test]
async fn pass_stream_as_another_type() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    store
        .run_concurrent(async |store| {
            let (input, _) = chunked_input(store, &[b"one", b"two"], false)?;
            let passed = instance
                .test_streams_echoes()
                .call_pass_as_bytes(store, input)
                .await?;
            let (bytes, ended) = read_bytes(store, passed, usize::MAX).await?;
            assert!(ended, "the stream must end");
            assert_eq!(bytes, b"onetwo");
            anyhow::Ok(())
        })
        .await??;
    Ok(())
}

/// A returned stream whose source can never produce more, with nothing else
/// left for the call's event loop to do, ends after what it produced, and its
/// source is cancelled.
#[tokio::test]
async fn idle_source_is_cancelled() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let cancelled = store
        .run_concurrent(async |store| {
            let echoes = instance.test_streams_echoes();
            let stream = echoes.call_idle_stream(store).await?;
            let (bytes, ended) = read_bytes(store, stream, usize::MAX).await?;
            assert!(ended, "the stream must end");
            assert_eq!(bytes, [1]);
            let mut cancelled = false;
            for _ in 0..1_000 {
                cancelled = echoes.call_idle_cancelled(store).await?;
                if cancelled {
                    break;
                }
                tokio::task::yield_now().await;
            }
            anyhow::Ok(cancelled)
        })
        .await??;
    assert!(cancelled, "the idle source must be cancelled");
    Ok(())
}

/// A promise lowered to a `future<u32>` that never settles does not keep the
/// call running: once the loop has nothing left that could settle it, the call
/// finishes.
#[tokio::test]
async fn never_settling_future_lets_the_call_finish() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let echoes = instance.test_streams_echoes();
    let drive = async |store: &Accessor<Ctx>| {
        for _ in 0..1000 {
            tokio::task::yield_now().await;
        }
        let _ = store;
    };
    store
        .run_concurrent(async |store| {
            let later = echoes.call_slow_future(store, 6).await?;
            assert_eq!(read_future(store, later).await?, 6);
            drive(store).await;
            anyhow::Ok(())
        })
        .await??;
    // `concurrent_state_table_size` is wasmtime's own leak check for tests: it
    // counts the store's live tasks and waitables.
    let settled = store.concurrent_state_table_size();
    store
        .run_concurrent(async |store| {
            let mut never = echoes.call_never_future(store).await?;
            never.close_with(store)?;
            drive(store).await;
            anyhow::Ok(())
        })
        .await??;
    // The future keeps entries of its own, since its writer stays registered for
    // a later settlement: 3 of them under wasmtime 49, against 10 for a call
    // whose task never finishes.
    assert_eq!(store.concurrent_state_table_size(), settled + 3);
    Ok(())
}

/// A promise lowered to a `future<u32>` that is garbage-collected without
/// settling is reported on stderr, since the future's writer then stays
/// unwritten for the instance's lifetime. The guest has no way to force a
/// collection, so it allocates garbage until a `WeakRef` to the promise is
/// cleared.
#[tokio::test]
async fn collected_unsettled_future_is_reported() -> anyhow::Result<()> {
    let (mut store, stderr) = store_capturing_stderr();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let echoes = instance.test_streams_echoes();
    let collected = store
        .run_concurrent(async |store| {
            let mut future = echoes.call_unreferenced_never_future(store).await?;
            let collected = echoes.call_collect_unreferenced_future(store).await?;
            future.close_with(store)?;
            anyhow::Ok(collected)
        })
        .await??;
    let stderr = shared::pipe_text(&stderr);
    assert!(collected, "the promise must be collected; stderr: {stderr}");
    assert!(
        stderr.contains(
            "Warning: a promise lowered to a future<u32> was garbage-collected without \
             settling, so the future's writable end leaks for the instance's lifetime"
        ),
        "stderr must report the leaked writer; got: {stderr}"
    );
    Ok(())
}

/// A future's value is lowered when its promise settles, so a wrapper disposed
/// of right after that is still delivered.
#[tokio::test]
async fn future_value_is_lowered_when_the_promise_settles() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let value =
        store
            .run_concurrent(async |store| {
                let future = instance
                    .test_streams_echoes()
                    .call_disposed_thing_future(store, "delivered".to_string())
                    .await?;
                let thing = read_future(store, future).await?;
                anyhow::Ok(store.with(|mut access| {
                    access.get().wasi.table().delete(thing).map(|thing| thing.0)
                })?)
            })
            .await??;
    assert_eq!(value, "delivered");
    Ok(())
}

/// Fulfill the promise `held-future` returned with 7, from a sync export if
/// `sync_settle`, and read the future, before the promise settles if
/// `read_first`. Returns the value read.
///
/// A sync export has no event loop, so the write it starts runs in the next
/// async call, which `ping` makes.
async fn settle_held_future(sync_settle: bool, read_first: bool) -> anyhow::Result<Option<u32>> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let echoes = instance.test_streams_echoes();
    let held = store
        .run_concurrent(async |store| echoes.call_held_future(store).await)
        .await??;
    let slot = Arc::new(Mutex::new(None));
    let settle_async = async |store: &Accessor<Ctx>| -> anyhow::Result<()> {
        if !sync_settle {
            echoes.call_settle_held_future_async(store, 7).await?;
        }
        Ok(())
    };
    if read_first {
        store
            .run_concurrent(async |store| {
                store.with(|store| held.pipe(store, OptionConsumer { slot: slot.clone() }))?;
                settle_async(store).await
            })
            .await??;
        if sync_settle {
            echoes.call_settle_held_future(&mut store, 7).await?;
        }
        store
            .run_concurrent(async |store| {
                echoes.call_ping(store, String::new()).await?;
                until(|| slot.lock().unwrap().is_some()).await;
                anyhow::Ok(())
            })
            .await??;
    } else {
        if sync_settle {
            echoes.call_settle_held_future(&mut store, 7).await?;
        }
        store
            .run_concurrent(async |store| {
                settle_async(store).await?;
                store.with(|store| held.pipe(store, OptionConsumer { slot: slot.clone() }))?;
                if sync_settle {
                    echoes.call_ping(store, String::new()).await?;
                }
                until(|| slot.lock().unwrap().is_some()).await;
                anyhow::Ok(())
            })
            .await??;
    }
    let value = *slot.lock().unwrap();
    Ok(value)
}

/// A promise lowered to a future that a later sync export fulfills is written in
/// the next async call, whether the host reads the future before or after that.
#[tokio::test]
async fn future_settled_by_a_sync_export_is_written() -> anyhow::Result<()> {
    assert_eq!(settle_held_future(true, false).await?, Some(7));
    assert_eq!(settle_held_future(true, true).await?, Some(7));
    Ok(())
}

/// A promise lowered to a future that a later async export fulfills is written,
/// whether the host reads the future before or after that.
#[tokio::test]
async fn future_settled_by_an_async_export_is_written() -> anyhow::Result<()> {
    assert_eq!(settle_held_future(false, false).await?, Some(7));
    assert_eq!(settle_held_future(false, true).await?, Some(7));
    Ok(())
}

/// Two async export calls on one instance, each taking a future the host
/// delivers later, both finish: each call's event loop reads its own future.
#[tokio::test]
async fn concurrent_calls_with_future_arguments_finish() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let delayed = |store: &Accessor<Ctx>, ms: u64| -> anyhow::Result<_> {
        Ok(store.with(|store| {
            FutureReader::new(store, async move {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                Ok::<_, std::io::Error>(Ok::<String, String>("v".to_string()))
            })
        })?)
    };
    let (first, second) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        store.run_concurrent(async |store| {
            let echoes = instance.test_streams_echoes();
            let (first, second) = futures::join!(
                echoes.call_describe_future_result(store, delayed(store, 100)?),
                echoes.call_describe_future_result(store, delayed(store, 300)?)
            );
            anyhow::Ok((first?, second?))
        }),
    )
    .await
    .expect("both calls must finish")??;
    assert_eq!(first, second);
    Ok(())
}
