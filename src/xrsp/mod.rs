//! Host-direct **XRSP (XR Streaming Protocol)** client — work in progress.
//!
//! Goal: subscribe to the Quest's flat "casting" video topic from Windows and
//! decode it with [`crate::decoder`], with no client-side lens de-warp. See
//! `docs/xrsp-protocol.md` for the reverse-engineering notes and milestones.
//!
//! Implemented so far: [`frame`] (the confirmed 8-byte packet framing).
//! Not yet: transport (USB FunctionFS `xrsp` / AirLink), the pairing + session
//! handshake, per-topic Cap'n Proto schemas, and AES-GCM topic crypto.
#![allow(dead_code)]

pub mod frame;
