<div align="center">

# AlloyVNC

### A VNC server that sends only what changed.

Capture that reports damage instead of polling, CopyRect for what moved, one update per client per turn, encoders that never touch an unchanged pixel.

[![license](https://img.shields.io/badge/license-MIT-2c6e8f)](LICENSE)

</div>

**AlloyVNC** is a VNC server in Rust, Windows first and Linux second, built
for low latency and few bytes rather than for feature count. It speaks RFB
3.8 to any viewer: TigerVNC, RealVNC, UltraVNC, noVNC in a browser.

## Status

Phase 0 of [the plan](docs/plan.md): the protocol, a synthetic screen, Raw
encoding, one client at a time. Nothing captures a real desktop yet.

```
cargo run -- serve
```

listens on `127.0.0.1:5900` with an animated test pattern. `--password`
turns on VNC authentication; a bind that is not loopback refuses to start
without one.

## Layout

| Crate | Holds |
|---|---|
| `alloyvnc-proto` | the wire format: pixel formats, messages, handshake, VNC auth; no I/O |
| `alloyvnc-region` | rectangles and y-x banded regions for damage |
| `alloyvnc-encode` | the framebuffer, pixel conversion, the encoders |
| `alloyvnc-screen` | the capture and input traits, and a synthetic screen |
| `alloyvnc` | the server, its sessions, a test client, and the binary |

The first four are pure and run their tests on any machine; the capture
backends for real screens get crates of their own.

## License

MIT, see [LICENSE](LICENSE).
