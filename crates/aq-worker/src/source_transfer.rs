//! Durable source transfer actions. Journal intent precedes network I/O; each
//! action is finite so the driver can service control while transport is pending.
#[cfg(feature = "synthetic-runtime")]
use crate::{clock::Clock, runtime::Runtime, source::GitClient};
use crate::{source::GitManifest, transfer_client::TransferClient};
use anyhow::{Context, Result, ensure};
use aq_worker_protocol::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
};
#[cfg(feature = "synthetic-runtime")]
use std::{io::Write, path::PathBuf};
use uuid::Uuid;

mod download;
mod files;
pub(crate) mod steps;
mod upload;
pub use download::*;
#[cfg(feature = "synthetic-runtime")]
pub(crate) use files::discard_incoming;
use files::read_range;
pub(crate) use files::{ArtifactAccess, artifact_file};
pub use upload::*;

pub(crate) fn matches_source_binding(manifest: &GitManifest, source: &SourceBinding) -> bool {
    manifest.repository_id == source.repository_id
        && manifest.repository_identity == source.logical_identity
        && manifest.binding_generation == source.binding_generation
        && manifest.base == source.base
}

fn next_chunk(
    manifest: &TransferManifest,
    manifest_offset: u64,
    bundle_offset: u64,
) -> Option<(TransferFile, u64, usize)> {
    let members = [
        (
            TransferFile::Manifest,
            manifest_offset,
            manifest.manifest_bytes,
        ),
        (TransferFile::Bundle, bundle_offset, manifest.bundle_bytes),
    ];
    members
        .into_iter()
        .find(|(_, offset, size)| offset < size)
        .map(|(file, offset, size)| {
            (
                file,
                offset,
                (size - offset).min(MAX_CHUNK_BYTES as u64) as usize,
            )
        })
}
