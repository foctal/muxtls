# Fuzzing

The fuzz workspace contains three `cargo-fuzz` targets:

- `varint_decode` checks arbitrary variable-length integer decoding and
  canonical round trips.
- `frame_decode` checks arbitrary frame decoding, encoded lengths, and
  canonical round trips.
- `connection_state` generates structured valid and invalid frame sequences and
  sends them through the real in-memory connection reader and state handling.

Fuzzing requires a nightly Rust toolchain, `cargo-fuzz`, and a supported
Unix-like platform:

```console
cargo install cargo-fuzz --locked
cargo +nightly fuzz run varint_decode
cargo +nightly fuzz run frame_decode
cargo +nightly fuzz run connection_state
```

Use `-max_total_time` for a bounded local run:

```console
cargo +nightly fuzz run connection_state -- -max_total_time=60
```

Crash inputs are written below `fuzz/artifacts/`. Re-run a saved input by
passing its path to the corresponding target.
