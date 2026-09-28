# audioe2e

A BORUIX audio end-to-end acceptance test: one blocking write-and-read-back round trip through real system calls.

[简体中文](README.md)

## What it tests

Two processes complete one round trip:

- The producer attaches an audio consumer slot, writes one frame of known PCM, and exits
- The consumer attaches the same slot, blocks waiting for the data, verifies it byte for byte, commits the consumption, and releases the slot

It covers two paths kernel boot-time tests cannot reach: the audio syscall wrappers themselves
(argument packing, error translation, user buffer checks), and the blocking wake round trip that
requires a real process context switch.

## Usage

The command word is `producer` or `consumer`; the two run as a pair, normally coordinated by
[`selftest`](https://github.com/BRX-Boruix/selftest):

```
[audioe2e] producer wrote full frame via VFS path
[audioe2e] consumer fetched full frame
[audioe2e] consumer verified payload byte-for-byte
[audioe2e] PASS: blocked reader woke, verified, committed
```

## Exit codes

- `0` — this side finished and all checks passed
- non-zero — failure; attach rejected, write rejected, wait exceeded and data mismatch are distinct, and the output line says which

## Building

```bash
cargo build --release
```

## Repository layout

```
audioe2e/
├── Cargo.toml    # package manifest
├── build.rs      # injects the linker script
├── linker.ld     # user-space segment layout
└── src/
    └── main.rs   # the producer and consumer round trip
```

## Related projects

- [`audiod`](https://github.com/BRX-Boruix/audiod) — the audio mixing daemon
- [`intel-hda`](https://github.com/BRX-Boruix/intel-hda) — the audio hardware driver
- [`selftest`](https://github.com/BRX-Boruix/selftest) — the host coordinating both sides

## License

MIT License, copyright Yang Borui. See [LICENSE](LICENSE).
