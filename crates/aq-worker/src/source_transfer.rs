//! Durable source transfer actions. Journal intent precedes network I/O; each
//! action is finite so the driver can service control while transport is pending.
use crate::{
    clock::Clock,
    runtime::Runtime,
    source::{GitClient, GitManifest},
    transfer_client::TransferClient,
};
use anyhow::{Context, Result, ensure};
use aq_worker_protocol::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
use uuid::Uuid;

mod download;
mod files;
mod upload;
pub use download::*;
pub(crate) use files::discard_incoming;
use files::{artifact_file, read_range};
pub use upload::*;
