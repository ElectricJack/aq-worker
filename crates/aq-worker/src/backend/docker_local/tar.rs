//! Docker archive helpers with placement backend errors.
use crate::backend::BackendError;
use aq_sandbox_archive::tar;
use zeroize::Zeroizing;

pub use tar::TreeEntry;

pub fn material_archive(
    path: &str,
    data: &[u8],
    uid: u32,
    gid: u32,
) -> Result<Zeroizing<Vec<u8>>, BackendError> {
    tar::material_archive(path, data, uid, gid).map_err(|e| BackendError::InvalidRequest(e.0))
}

pub fn single_file(archive: &[u8], max: u64) -> Result<Vec<u8>, BackendError> {
    tar::single_file(archive, max).map_err(|e| BackendError::InvalidRequest(e.0))
}

pub fn tree_archive(prefix: &str, entries: &[TreeEntry]) -> Result<Vec<u8>, BackendError> {
    tar::tree_archive(prefix, entries).map_err(|e| BackendError::InvalidRequest(e.0))
}

pub fn read_tree(archive: &[u8], max_total: u64) -> Result<Vec<TreeEntry>, BackendError> {
    tar::read_tree(archive, max_total).map_err(|e| BackendError::InvalidRequest(e.0))
}
