# Zarr V2 dtype endianness in ziv

Why `crates/zarr-core/src/image.rs` registers four extra Zarr V2 dtype aliases with the `zarrs`
plugin registry at open time, and why it does *not* rewrite dtype strings or fork zarrs' open
path. Recorded here because the obvious implementation is wrong in a way that is invisible until
you read the upstream source, and the first implementation shipped in this repo made exactly that
mistake.

## The bug

`ziv` rejected OME-Zarr images whose `.zarray` declared a big-endian dtype with
`Error: Open("data type >u1 is not supported")`. The natural hypothesis is that ziv's own
`classify_dtype` — which compares a `zarrs::array::DataType` against `uint8()`, `uint16()` and so
on — is too strict for a big-endian-parsed type.

**That hypothesis is wrong, and it matters, because acting on it produces a fix that does
nothing.**

## What is actually true

Two findings, both verified against the `zarrs` 0.23.13 source *and* empirically with
hand-authored fixtures, rather than by reading either alone.

### Multi-byte big-endian types already work end to end

`zarrs-0.23.13/src/array/data_type/uint.rs` registers both endian prefixes as aliases of the
*same* data type:

```rust
zarrs_plugin::impl_extension_aliases!(UInt16DataType,
    v3: "uint16", [],
    v2: "<u2", ["<u2", ">u2"]
);
```

So `DataType::from_metadata_v2("<u2")` and `from_metadata_v2(">u2")` produce identical values, and
`classify_dtype`'s equality comparison already succeeds for `>u2`. Every multi-byte numeric type
(`u2/u4/u8`, `i2/i4/i8`, `f2/f4/f8`) follows this pattern.

Endianness itself is tracked separately, in the codec: `array_metadata_v2_to_v3` parses the
leading prefix character and threads it into the V3 `bytes` array-to-bytes codec's `endian`
config, whose decode path byte-swaps every component before `retrieve_array_subset` ever sees the
bytes. `tests/fixtures/sample_be_u2.ome.zarr` and `sample_be_i4.ome.zarr` are authored with
genuinely big-endian chunk bytes (`to_be_bytes`) specifically to prove this empirically — if the
codec did not swap, `x=15` would decode as `15 << 8 == 3840`.

**No ziv change was ever needed for multi-byte big-endian data.**

### The real gap is one-byte types, and it is a name-resolution failure

`UInt8DataType` and `Int8DataType` are the exception: each registers a **single** V2 alias.

```rust
zarrs_plugin::impl_extension_aliases!(UInt8DataType,
    v3: "uint8", [],
    v2: "|u1", ["|u1"]      // no ">u1", no "<u1"
);
```

`>u1`, `<u1`, `>i1` and `<i1` are not registered at all. When `DataType::from_metadata_v2` looks
up `">u1"` no plugin matches, and it returns `PluginUnsupportedError`, whose `Display` is exactly
`"data type >u1 is not supported"`. This happens **inside `Array::open`, during metadata parsing,
before `classify_dtype` is ever called** — which is why loosening `classify_dtype` cannot work.

Endianness is meaningless for a single byte (`>u1`, `<u1` and `|u1` describe byte-identical data),
so the prefixed forms are spec-nonsensical, but real-world writers emit them.

## The fix, and the one that was wrong

The fix must land in dtype **name resolution**, upstream of `Array::open`.

`zarrs` exposes exactly that hook: V2 data-type plugins match names through a runtime-mutable
alias list (`zarrs::plugin::ExtensionAliasesV2`, re-exported from `zarrs_plugin`). Registering the
four prefixed spellings as first-class aliases makes plain `Array::open` and `Array::async_open`
accept them, with no change to the open path at all. Registration is `Once`-guarded because the
registry is process-global behind an `RwLock`.

**The first implementation did something else**, and it is worth recording why it was replaced. It
hand-rolled a `.zarray` fetch, patched the dtype string to `|u1`, merged `.zattrs` itself, and
constructed the array via `Array::new_with_metadata` — in a sync copy *and* a near-identical async
copy. That worked, but it:

- reimplemented part of zarrs' own `open_metadata`, creating a standing obligation to re-diff
  against upstream on every `zarrs` bump;
- duplicated 35 lines across the sync and async paths, which is how the async half ended up
  untested (a mutation deleting it left the entire suite green);
- fetched `zarr.json`, discarded the body, then let `Array::open` fetch it again — one redundant
  network round trip per pyramid level on the V3 path;
- made `arr.metadata()` report a rewritten `|u1` instead of the true on-disk dtype.

All four disappear on the alias route. The lesson is narrow but general: when an upstream library
rejects your input, check whether it exposes a registration hook before reimplementing its parser.

## What is deliberately not aliased

Only the four one-byte spellings. Aliasing multi-byte prefixes would be unnecessary (they already
work) and, done carelessly — for example with a broad regex like `^[<>|].1$` — actively wrong: it
would also swallow `>b1`, `|S1`, `>V1` and `>f1`, silently opening each as `uint8` and misreading
the pixels. `crates/zarr-core/tests/one_byte_endian_aliases.rs` asserts those four stay refused,
precisely so a future "simplification" to a regex fails the suite.

## Tests that pin this

| Property | Where |
|---|---|
| All four spellings open through the local **and** remote paths | `tests/one_byte_endian_aliases.rs` |
| The async registration specifically (its own process, since the registry is global) | `tests/remote_only_one_byte_endian.rs` |
| Multi-byte big-endian decodes to correct values, not byte-swapped garbage | `image.rs` unit tests + the `sample_be_*` fixtures |
| Aliasing did not make dtype resolution permissive | `tests/one_byte_endian_aliases.rs` |
