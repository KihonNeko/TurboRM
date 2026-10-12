#![forbid(unsafe_code)]

//! Reusable cloud/document core for the CLI and Linux desktop client.
pub mod auth;
pub mod cloud;
pub mod cover;
pub mod folders;
pub mod hash_tree;
pub mod import;
pub mod job;
mod legacy_notebook;
pub mod library;
pub mod manifest;
pub mod moves;
pub mod notebook;
pub mod scene_text;
pub mod transfer;
