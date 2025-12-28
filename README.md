# http-stream-proxy

> ⚠️ **Experimental project — no stability guarantees**

`http-stream-proxy` is a small, experimental HTTP proxy that rewrites M3U playlists and fan-outs upstream media streams to multiple downstream consumers.

The project is intended as a **technical experiment** in:
- async Rust
- Axum
- HTTP streaming
- single-producer / multi-consumer fan-out
- playlist rewriting

It is **not a production-ready system** and makes **no guarantees** about stability, performance, or correctness.

---

## What this project does

- Fetches an upstream M3U playlist at startup
- Parses playlist metadata into memory
- Rewrites playlist entries so that media URLs point back to the proxy
- For each unique upstream stream:
  - Maintains **a single upstream connection**
  - Broadcasts data to multiple downstream clients
- Automatically shuts down upstream connections when no consumers remain

This allows multiple clients to watch the same stream without opening multiple upstream connections.

---

## What this project does *not* do

- It does **not** provide any media content
- It does **not** include playlists, credentials, or provider URLs
- It does **not** bypass DRM, encryption, or authentication
- It does **not** attempt to hide or obfuscate traffic
- It does **not** emulate proprietary IPTV systems

This is a **generic HTTP/MPEG-TS proxy**.

---

## Legal notice / disclaimer

This software is provided **as-is**, for educational and experimental purposes only.

The authors:
- Do **not** provide any media streams or playlists
- Do **not** endorse or encourage copyright infringement
- Take **no responsibility** for how this software is used

You are responsible for ensuring that you have the **legal right** to access, proxy, and redistribute any streams configured with this software.

If you do not have such rights, **do not use this software**.

---

## Experimental status

⚠️ **This project is experimental.**

- APIs may change at any time
- No backward compatibility is guaranteed
- Error handling is intentionally minimal in places
- No formal security review has been performed

Use at your own risk.

---

## Configuration

The proxy is configured at startup (e.g. via a config file or environment variables):

- Upstream M3U URL
- Listen address / port
- Optional HTTP client settings (timeouts, headers)

The proxy does **not** dynamically fetch playlists per request.

---

## Development goals

This project exists primarily to explore:

- Efficient stream fan-out in async Rust
- Correct lifecycle management of upstream connections
- Practical Axum streaming patterns
- Clean separation between parsing, state, and HTTP layers

---

## License

This project is licensed under the MIT License.  
See the [LICENSE](LICENSE) file for details.
