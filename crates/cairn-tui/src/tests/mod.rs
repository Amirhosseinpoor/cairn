//! Golden screens and key interaction, drawn into a buffer and read back.
//! They live inside the crate: nothing outside `cairn-cli` may name it
//! (T-ARCH-003).

mod interact;
mod screens;
