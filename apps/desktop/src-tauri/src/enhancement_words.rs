// SPDX-License-Identifier: MIT
//! What the Enhancement card says about a provider that cannot be used,
//! worded here and nowhere else (user-facing wording lives in one
//! place per app). `macalla-enhancers` reports Apple Intelligence's state as
//! a typed `Availability`; `commands.rs` picks an entry here by that state and
//! the platform, and never builds one.
//!
//! Each entry is a whole sentence. The platform is not a placeholder: each
//! platform has its own entry, because other languages may word them apart.

/// Apple Intelligence on a Windows build, which can never have it.
pub(crate) const APPLE_INTELLIGENCE_NOT_ON_WINDOWS: &str =
    "Apple Intelligence is not available on Windows.";

/// Apple Intelligence on a Linux build, which can never have it.
pub(crate) const APPLE_INTELLIGENCE_NOT_ON_LINUX: &str =
    "Apple Intelligence is not available on Linux.";
