// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Integration tests for WebIDL parameterized types:
//! - `Vec<T>` (sequence)
//! - `Record<V>` (record)
//! - `#[webidl_union]` (union)
//! - `AsyncSequence` (async iterable)

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

/// Run [`eval_with_setup`](core_runtime::test_util::eval_with_setup) on `code` with `r(f)`
/// defined. `r(f)` returns `String(f())`, or `throws:` followed by the constructor name of the
/// exception `f` threw.
fn eval_outcomes(setup: impl FnOnce(), code: &str) -> String {
    let prelude =
        "const r = f => { try { return String(f()); } catch (e) { return `throws:${e.constructor.name}`; } };";
    core_runtime::test_util::eval_with_setup(setup, &format!("{prelude}\n{code}"))
}

// ============================================================================
// Vec<T> (sequence) tests
// ============================================================================

mod sequence_tests {
    use core_runtime::jsclass;
    use core_runtime::jsmethods;

    #[jsclass]
    struct SeqAcceptor {}

    #[jsmethods]
    impl SeqAcceptor {
        #[constructor]
        fn construct() -> Self {
            Self {}
        }

        /// Accept a sequence of strings and join them.
        #[method]
        fn join_strings(&self, items: Vec<String>) -> String {
            items.join(",")
        }

        /// Accept a sequence of integers and sum them.
        #[method]
        fn sum_ints(&self, items: Vec<i32>) -> i32 {
            items.iter().sum()
        }

        /// Accept an optional sequence.
        #[method]
        fn optional_seq(&self, items: Option<Vec<String>>) -> String {
            match items {
                Some(v) => v.join(","),
                None => "none".to_string(),
            }
        }

        /// Static method accepting a sequence.
        #[static_method]
        fn count(items: Vec<String>) -> u32 {
            items.len() as u32
        }
    }

    fn eval(code: &str) -> String {
        super::eval_outcomes(
            || {
                core_runtime::runtime::register_global_initializer(|scope, global| {
                    SeqAcceptor::add_to_global(scope, global);
                });
            },
            code,
        )
    }

    #[test]
    fn sequences_convert_from_any_iterable() {
        assert_eq!(
            eval(
                "const s = new SeqAcceptor();
                 [r(() => s.joinStrings(['a', 'b'])), r(() => s.joinStrings([])),
                  r(() => s.sumInts([1, 2, 3])), r(() => s.joinStrings(new Set(['x', 'y']))),
                  r(() => SeqAcceptor.count(['a', 'b'])), r(() => s.joinStrings(42))].join('|')"
            ),
            "a,b||6|x,y|2|throws:TypeError"
        );
    }

    /// `null` is a present value, which isn't iterable. Only `undefined` makes the argument
    /// absent.
    #[test]
    fn optional_sequences_are_absent_only_for_undefined() {
        assert_eq!(
            eval(
                "const s = new SeqAcceptor();
                 [r(() => s.optionalSeq(['x', 'y'])), r(() => s.optionalSeq(undefined)),
                  r(() => s.optionalSeq(null))].join('|')"
            ),
            "x,y|none|throws:TypeError"
        );
    }
}

// ============================================================================
// Record<V> tests
// ============================================================================

mod record_tests {
    use core_runtime::jsclass;
    use core_runtime::jsmethods;
    use js::Record;

    #[jsclass]
    struct RecordAcceptor {}

    #[jsmethods]
    impl RecordAcceptor {
        #[constructor]
        fn construct() -> Self {
            Self {}
        }

        /// Accept a record of string values, return keys joined.
        #[method]
        fn keys(&self, rec: Record<String, String>) -> String {
            rec.keys().cloned().collect::<Vec<_>>().join(",")
        }

        /// Accept a record of string values, return values joined.
        #[method]
        fn values(&self, rec: Record<String, String>) -> String {
            rec.values().cloned().collect::<Vec<_>>().join(",")
        }

        /// Accept an optional record.
        #[method]
        fn optional_record(&self, rec: Option<Record<String, String>>) -> String {
            match rec {
                Some(r) => r.keys().cloned().collect::<Vec<_>>().join(","),
                None => "none".to_string(),
            }
        }

        /// Echo the record back to JS (exercises `Record::to_jsval`).
        #[method]
        fn echo(&self, rec: Record<String, String>) -> Record<String, String> {
            rec
        }
    }

    fn eval(code: &str) -> String {
        super::eval_outcomes(
            || {
                core_runtime::runtime::register_global_initializer(|scope, global| {
                    RecordAcceptor::add_to_global(scope, global);
                });
            },
            code,
        )
    }

    /// Every enumerable own key is converted to the key type, integer keys first in ascending
    /// order, and a symbol key throws. Non-enumerable keys are skipped. `null` is a present value,
    /// which isn't an object.
    #[test]
    fn records_convert_enumerable_own_keys_in_order() {
        assert_eq!(
            eval(
                "const a = new RecordAcceptor();
                 const sym = {a: '1'}; sym[Symbol('x')] = '2';
                 const hidden = {a: '1'};
                 Object.defineProperty(hidden, 'b', {value: '2', enumerable: false});
                 [r(() => a.keys({z: '1', a: '2', m: '3'})), r(() => a.values({x: 'hi', y: 'yo'})),
                  r(() => a.keys({})), r(() => a.keys({1: 'a', 0: 'b'})), r(() => a.keys(hidden)),
                  r(() => a.keys(sym)), r(() => a.keys(42)), r(() => a.optionalRecord({k: 'v'})),
                  r(() => a.optionalRecord(null))].join('|')"
            ),
            "z,a,m|hi,yo||0,1|a|throws:TypeError|throws:TypeError|k|throws:TypeError"
        );
    }

    /// Non-ASCII and NUL code units in keys survive the round trip through Rust.
    #[test]
    fn record_keys_round_trip() {
        assert_eq!(
            eval(
                "const e = new RecordAcceptor().echo({'é🦊': 'vé', 'a\\u0000b': 'v'}); \
                 Object.keys(e).map(k => k.length).join(',') + ':' + e['é🦊'] + e['a\\u0000b']"
            ),
            "3,3:vév"
        );
    }
}

// ============================================================================
// Union type tests
// ============================================================================

mod union_tests {
    use core_runtime::jsclass;
    use core_runtime::jsmethods;
    use core_runtime::webidl_union;

    #[webidl_union]
    pub enum StringOrLong {
        Str(String),
        Long(i32),
    }

    #[webidl_union]
    pub enum StringOrBool {
        Str(String),
        Bool(bool),
    }

    // No string member: the numeric member is the §3.2.25 fallback (ToNumber).
    #[webidl_union]
    pub enum BoolOrLong {
        Bool(bool),
        Long(i32),
    }

    // No string or numeric member: the boolean member is the fallback (ToBoolean).
    #[webidl_union]
    pub enum SeqOrBool {
        Items(Vec<String>),
        Flag(bool),
    }

    #[jsclass]
    struct UnionAcceptor {}

    #[jsmethods]
    impl UnionAcceptor {
        #[constructor]
        fn construct() -> Self {
            Self {}
        }

        #[method]
        fn string_or_long(&self, val: StringOrLong) -> String {
            match val {
                StringOrLong::Str(s) => format!("string:{s}"),
                StringOrLong::Long(n) => format!("long:{n}"),
            }
        }

        #[method]
        fn string_or_bool(&self, val: StringOrBool) -> String {
            match val {
                StringOrBool::Str(s) => format!("string:{s}"),
                StringOrBool::Bool(b) => format!("bool:{b}"),
            }
        }

        #[method]
        fn optional_union(&self, val: Option<StringOrLong>) -> String {
            match val {
                Some(StringOrLong::Str(s)) => format!("string:{s}"),
                Some(StringOrLong::Long(n)) => format!("long:{n}"),
                None => "none".to_string(),
            }
        }

        #[method]
        fn bool_or_long(&self, val: BoolOrLong) -> String {
            match val {
                BoolOrLong::Bool(b) => format!("bool:{b}"),
                BoolOrLong::Long(n) => format!("long:{n}"),
            }
        }

        #[method]
        fn seq_or_bool(&self, val: SeqOrBool) -> String {
            match val {
                SeqOrBool::Items(v) => format!("seq:{}", v.len()),
                SeqOrBool::Flag(b) => format!("flag:{b}"),
            }
        }
    }

    fn eval(code: &str) -> String {
        super::eval_outcomes(
            || {
                core_runtime::runtime::register_global_initializer(|scope, global| {
                    UnionAcceptor::add_to_global(scope, global);
                });
            },
            code,
        )
    }

    /// A number picks the numeric member and a boolean the boolean member over a string member.
    /// `null` is a present value, which the string member converts. Without a string member, the
    /// numeric member is the fallback, and without either, the boolean member is.
    #[test]
    fn unions_pick_the_member_the_value_converts_to() {
        assert_eq!(
            eval(
                "const u = new UnionAcceptor();
                 [u.stringOrLong(3.14), u.stringOrLong('hello'), u.stringOrBool(false),
                  u.stringOrBool('test'), u.optionalUnion(null), u.optionalUnion(undefined),
                  u.optionalUnion(99), u.boolOrLong('5'), u.boolOrLong(true), u.seqOrBool(5),
                  u.seqOrBool(0), u.seqOrBool(['a', 'b'])].join('|')"
            ),
            "long:3|string:hello|bool:false|string:test|string:null|none|long:99|long:5|bool:true|\
             flag:true|flag:false|seq:2"
        );
    }
}

mod union_lifetime_tests {
    use core_runtime::jsclass;
    use core_runtime::jsmethods;
    use core_runtime::webidl_union;
    use js::ArrayBuffer;
    use js::Object;

    /// Union with a scope-rooted variant and a primitive variant.
    #[webidl_union]
    pub enum BufferOrString<'a> {
        Buffer(ArrayBuffer<'a>),
        Str(String),
    }

    /// Union with two scope-rooted variants of different specificity. The
    /// `ArrayBuffer` branch is declared first so it wins for typed buffers;
    /// other objects fall through to the generic `Object` branch.
    #[webidl_union]
    pub enum BufferOrObject<'a> {
        Buffer(ArrayBuffer<'a>),
        Obj(Object<'a>),
    }

    #[jsclass]
    struct LifetimeUnionAcceptor {}

    #[jsmethods]
    impl LifetimeUnionAcceptor {
        #[constructor]
        fn construct() -> Self {
            Self {}
        }

        #[method]
        fn buffer_or_string(&self, val: BufferOrString<'_>) -> String {
            match val {
                BufferOrString::Buffer(b) => format!("buffer:{}", b.byte_length()),
                BufferOrString::Str(s) => format!("string:{s}"),
            }
        }

        #[method]
        fn optional_buffer_or_string(&self, val: Option<BufferOrString<'_>>) -> String {
            match val {
                Some(BufferOrString::Buffer(b)) => format!("buffer:{}", b.byte_length()),
                Some(BufferOrString::Str(s)) => format!("string:{s}"),
                None => "none".to_string(),
            }
        }

        #[method]
        fn buffer_or_object(&self, val: BufferOrObject<'_>) -> String {
            match val {
                BufferOrObject::Buffer(b) => format!("buffer:{}", b.byte_length()),
                BufferOrObject::Obj(_) => "object".to_string(),
            }
        }
    }

    fn eval(code: &str) -> String {
        super::eval_outcomes(
            || {
                core_runtime::runtime::register_global_initializer(|scope, global| {
                    LifetimeUnionAcceptor::add_to_global(scope, global);
                });
            },
            code,
        )
    }

    /// A value that doesn't match the `ArrayBuffer` member falls back to the string member. The
    /// members are tried in declaration order. A union without a fallback member throws.
    #[test]
    fn unions_with_rooted_members_convert() {
        assert_eq!(
            eval(
                "const u = new LifetimeUnionAcceptor();
                 [r(() => u.bufferOrString(new ArrayBuffer(8))), r(() => u.bufferOrString(42)),
                  r(() => u.bufferOrString({})), r(() => u.optionalBufferOrString(null)),
                  r(() => u.optionalBufferOrString(undefined)),
                  r(() => u.optionalBufferOrString(new ArrayBuffer(4))),
                  r(() => u.bufferOrObject(new ArrayBuffer(16))), r(() => u.bufferOrObject({a: 1})),
                  r(() => u.bufferOrObject('not an object'))].join('|')"
            ),
            "buffer:8|string:42|string:[object Object]|string:null|none|buffer:4|buffer:16|object|\
             throws:TypeError"
        );
    }
}

// ============================================================================
// Sequence-or-record union tests (e.g. URLSearchParams init shape)
// ============================================================================

mod union_sequence_record_tests {
    use core_runtime::jsclass;
    use core_runtime::jsmethods;
    use core_runtime::webidl_union;
    use js::conversion::Record;

    /// Mirrors the WebIDL signature
    /// `(sequence<sequence<USVString>> or record<USVString, USVString> or USVString)`
    /// used by `URLSearchParams`.
    #[webidl_union]
    pub enum SeqOrRecordOrString {
        Pairs(Vec<Vec<String>>),
        Record(Record<String, String>),
        Str(String),
    }

    #[jsclass]
    struct SeqRecordAcceptor {}

    #[jsmethods]
    impl SeqRecordAcceptor {
        #[constructor]
        fn construct() -> Self {
            Self {}
        }

        #[method]
        fn classify(&self, val: SeqOrRecordOrString) -> String {
            match val {
                SeqOrRecordOrString::Pairs(pairs) => {
                    let mut parts = Vec::new();
                    for pair in pairs {
                        parts.push(pair.join("="));
                    }
                    format!("pairs:{}", parts.join("&"))
                }
                SeqOrRecordOrString::Record(record) => {
                    let mut entries: Vec<String> = record
                        .into_iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    entries.sort();
                    format!("record:{}", entries.join("&"))
                }
                SeqOrRecordOrString::Str(s) => format!("string:{s}"),
            }
        }
    }

    fn eval(code: &str) -> String {
        super::eval_outcomes(
            || {
                core_runtime::runtime::register_global_initializer(|scope, global| {
                    SeqRecordAcceptor::add_to_global(scope, global);
                });
            },
            code,
        )
    }

    /// An iterable picks the sequence member, and then an element that isn't a sequence, or an
    /// iterator that throws, makes the conversion throw rather than fall through to the record.
    #[test]
    fn sequence_or_record_unions_pick_by_iterability() {
        assert_eq!(
            eval(
                "const a = new SeqRecordAcceptor();
                 const it = { [Symbol.iterator]: function*() { yield ['a','1']; yield ['b','2']; } };
                 const bad = { [Symbol.iterator]() { return { next() { throw new Error('boom'); } }; } };
                 [r(() => a.classify([['a','1'], ['b','2']])), r(() => a.classify({a:'1', b:'2'})),
                  r(() => a.classify('hello')), r(() => a.classify(it)), r(() => a.classify([1, 2])),
                  r(() => a.classify(bad))].join('|')"
            ),
            "pairs:a=1&b=2|record:a=1&b=2|string:hello|pairs:a=1&b=2|throws:TypeError|throws:Error"
        );
    }
}

// ============================================================================
// AsyncSequence tests
// ============================================================================

mod async_sequence_tests {
    use core_runtime::jsclass;
    use core_runtime::jsmethods;
    use js::AsyncSequence;

    #[jsclass]
    struct AsyncAcceptor {}

    #[jsmethods]
    impl AsyncAcceptor {
        #[constructor]
        fn construct() -> Self {
            Self {}
        }

        /// Accept an async iterable and report whether it's async.
        #[method]
        fn is_async(&self, seq: AsyncSequence) -> bool {
            seq.is_async()
        }

        /// Accept an optional async iterable.
        #[method]
        fn optional_async(&self, seq: Option<AsyncSequence>) -> String {
            match seq {
                Some(s) => if s.is_async() { "async" } else { "sync" }.to_string(),
                None => "none".to_string(),
            }
        }
    }

    fn eval(code: &str) -> String {
        super::eval_outcomes(
            || {
                core_runtime::runtime::register_global_initializer(|scope, global| {
                    AsyncAcceptor::add_to_global(scope, global);
                });
            },
            code,
        )
    }

    /// A getter of `Symbol.asyncIterator` that throws, and a non-callable `Symbol.asyncIterator`,
    /// throw rather than fall back to `Symbol.iterator`.
    #[test]
    fn async_sequences_detect_async_iterables() {
        assert_eq!(
            eval(
                "const a = new AsyncAcceptor();
                 const sync = { [Symbol.iterator]() { return { next() { return { done: true } } } } };
                 const asyncIt = { [Symbol.asyncIterator]() { return { next() { return { done: true } } } } };
                 const trap = { get [Symbol.asyncIterator]() { throw new RangeError('trap') }, \
                                [Symbol.iterator]: sync[Symbol.iterator] };
                 const notCallable = { [Symbol.asyncIterator]: 42, [Symbol.iterator]: sync[Symbol.iterator] };
                 [r(() => a.isAsync([1, 2])), r(() => a.isAsync(asyncIt)), r(() => a.optionalAsync([1])),
                  r(() => a.optionalAsync(null)), r(() => a.isAsync(42)), r(() => a.isAsync({})),
                  r(() => a.isAsync(trap)), r(() => a.isAsync(notCallable))].join('|')"
            ),
            "false|true|sync|throws:TypeError|throws:TypeError|throws:TypeError|throws:RangeError|\
             throws:TypeError"
        );
    }
}
