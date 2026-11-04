# Bounded read-only formatting audit

No code was run, and this is not runtime proof. Scope: the new modeled row calls
rowsize::btree_size, then relsize::index_tuple/IndexTupleSize::push and format_value.
The audit read those functions, format_plain, array and range formatting, relevant
thread-locals, FormatCtx, and the native output dispatch. It did not exhaustively
prove every type or native output callback.

format_value and format_plain receive FormatCtx explicitly. Date/time settings,
extra_float_digits, bytea mode, name resolution and formatting settings are fields
of that cloned object. NameResolver requires Send + Sync. Array output passes the
same context recursively. range::format/format_multi pass context to format_plain;
the range COLL thread-local is used in bound comparison, not those output functions.
The tsearch INPUT_NOTICES thread-local is for input notices; the inspected output
functions do not use it. Native outputs are dispatched through a static TYPES table
and receive FormatCtx; not every callback body was followed exhaustively.

A concrete dependency remains outside that explicit context:

- catalog/relsize.rs::varlena_payload, Value::Text/Json non-ASCII branch, calls
  mbconv::server::octet_len(collation::database_encoding(), s).
- types/collation/mod.rs::database_encoding reads the DEFAULT thread-local,
  documented and implemented to return UTF8 on a thread where no catalog was noted.
- binder::note_catalog installs a catalog default on its current thread. The scoped
  roll-build worker closure in parbuild.rs does not install that default. Its Plan
  has FormatCtx, including server_encoding, but the sizing call above does not use
  that field.
- mbconv::server::octet_len returns UTF8 byte count for UTF8 and character count for
  representable single-byte encodings. Therefore the source contains a concrete
  route to different non-ASCII sizes on a fresh worker versus the leader in a
  non-UTF8 database. This was not reproduced. The measured path already calls the
  same sizing code; the modeled path newly reaches it. Array/nested values may also
  reach the same varlena sizing branch.

No formatting or locale propagation fix was folded into the two accepted MEDIUM
corrections. A focused non-UTF8 serial/worker fixture and owner decision are still
required; UTF8 ASCII fixtures do not establish this boundary. This is stronger
source evidence than the original unverified LOW, not a new executed failure or a
claim that all other formatting is worker-safe.
