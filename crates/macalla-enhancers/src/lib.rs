// SPDX-License-Identifier: MIT
//! Tier 1 providers behind [`macalla_core::enhance::Enhancer`].
//!
//! No model runs inside Macalla. A provider is a model the person already
//! has: Apple Intelligence on a Mac that has it ([`apple`]), an
//! OpenAI-compatible endpoint they run or pay for ([`endpoint`]), or
//! Anthropic's own Messages API ([`anthropic`]) for the reasoning control its
//! OpenAI-compatible layer does not honour. The prompt and the guard on what
//! comes back belong to the core; this crate only turns them into a request
//! and a reply.

pub mod anthropic;
pub mod apple;
pub mod endpoint;
pub mod gemini;
pub mod mistral;
pub mod openai;
pub mod openrouter;
mod provider_error;
#[cfg(test)]
mod test_support;
