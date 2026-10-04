// SPDX-License-Identifier: MIT
//! Translation backends behind [`macalla_core::translate::Translator`].
//!
//! The rules -- exactly one translation per text, matched by index; stop between texts; blank
//! text left alone -- belong to the core's `translate_all`. A backend here
//! only turns a batch into its own request and hands each answer back.
//!
//! Only Apple's on-device translation for now ([`apple`]). A cloud provider,
//! an Enhancement provider or a local model would each be a module and a
//! feature of their own; any that sends text off the machine must be
//! labelled as doing so where the app offers it (nothing leaves the machine unless the user turned it on).

pub mod apple;
