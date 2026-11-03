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
pub(crate) use files::artifact_file;
#[cfg(feature = "synthetic-runtime")]
pub(crate) use files::discard_incoming;
use files::read_range;
pub use upload::*;
