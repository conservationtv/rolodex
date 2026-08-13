#![forbid(unsafe_code)]

mod event;
mod identity;
mod key;
mod manifest;

#[cfg(target_arch = "wasm32")]
mod queue;
#[cfg(target_arch = "wasm32")]
mod r2_source;
