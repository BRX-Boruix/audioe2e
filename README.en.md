# audioe2e

An **end-to-end acceptance program** for BORUIX, verifying that **blocking wait and wake-up round trips** in the audio domain work from real user space.

[简体中文](README.md)

## What it tests

The program has two roles (selected by startup argument) and runs one complete audio data round trip through **real syscalls**:

| Role | Behaviour |
| --- | --- |
| `producer` | Attaches as a consumer, writes one frame of known PCM through a file path, exits |
| `consumer` | Attaches as a consumer, **blocks waiting** for data, verifies it byte by byte, then commits, detaches, and exits |

The writing side fills a **deterministic sequence** (crossing the 0/255 boundary, with periodicity), so a byte-order or offset error is bound to show up — it is not "write some data and call it a pass".

There is also a **two-stream mixing** check: the two streams write square waves of opposite sign whose amplitudes differ by a factor of three; summed and passed through a fixed gain, the expected result is about `8192`.

> Using the same sequence for both streams would be **undecidable** — summed and halved it reconstructs the original exactly, identical to mixing only one stream or passing it straight through. The amplitudes must differ markedly for the check to mean anything.

## Why a separate program

The in-kernel self-test calls the audio node and ring buffer directly, running in kernel mode with kernel-owned buffers, and **cannot reach** two critical paths:

1. **The syscall wrappers themselves** — argument packing, error code translation, user buffer validation;
2. **A real blocking round trip** — which requires a genuine process context switch and can only be verified in a user-space process with a process context.

This program exists to cover those two.

## Usage

The coordinating side starts both roles (`producer` / `consumer`). The exit code is the verdict.

## Exit codes

| Exit code | Meaning |
| --- | --- |
| `0` | Everything correct |
| Non-zero | An honest failure (the coordinating side asserts on it and never silences a failure into a success) |

## Building

```bash
cargo build --release
```

## Layout

```
audioe2e/
├── Cargo.toml    # package definition
├── build.rs      # injects the linker script
├── linker.ld     # user-space section layout
└── src/
    └── main.rs   # producer and consumer logic
```

## Related projects

- [`libsys`](https://github.com/BRX-Boruix/libsys) — provides the audio-domain syscall wrappers
- [`audiod`](https://github.com/BRX-Boruix/audiod) — the audio mixing daemon
- [`audiofile`](https://github.com/BRX-Boruix/audiofile) — the WAV player

## License

MIT License, copyright Yang Borui. See [LICENSE](LICENSE).
