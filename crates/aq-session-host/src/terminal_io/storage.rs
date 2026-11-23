//! Authenticated on-disk terminal state and request envelopes.
use super::*;
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
pub(super) fn load(dir: &Path, binding: &TerminalBinding) -> Result<State> {
    binding.validate()?;
    let plaintext = unprotect(
        dir,
        binding,
        b"state",
        &private_bytes(&dir.join(STATE), MAX_LOCAL_FILE)?,
    )?;
    let state: State = serde_json::from_slice(&plaintext)
        .map_err(|_| anyhow::anyhow!("invalid terminal ledger"))?;
    ensure!(
        state.revision > 0
            && state.output_claim.is_none_or(|id| !id.is_nil())
            && state.binding == *binding
            && state.input.len() <= MAX_INPUT_RECORDS
            && state.control.len() <= MAX_CONTROL_RECORDS,
        "terminal ledger binding/bounds"
    );
    Ok(state)
}
pub(super) fn private_bytes(path: &Path, max: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.nlink() == 1
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o777 == 0o600
            && m.len() <= max,
        "unsafe terminal material"
    );
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= max,
        "terminal material grew beyond bound"
    );
    Ok(bytes)
}
pub(super) fn private_json<T: serde::de::DeserializeOwned>(path: &Path, max: u64) -> Result<T> {
    serde_json::from_slice(&private_bytes(path, max)?)
        .map_err(|_| anyhow::anyhow!("invalid terminal metadata"))
}
pub(super) fn key(dir: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let key = Zeroizing::new(private_bytes(&dir.join("terminal.key"), 32)?);
    ensure!(key.len() == 32, "terminal key length");
    Ok(key)
}
fn aad(binding: &TerminalBinding, purpose: &[u8]) -> Result<Vec<u8>> {
    let mut aad = b"aq.guarded-terminal-request/1\0".to_vec();
    aad.extend_from_slice(purpose);
    aad.push(0);
    aad.extend_from_slice(typed_digest(binding)?.as_str().as_bytes());
    Ok(aad)
}
pub(super) fn typed_digest<T: Serialize>(value: &T) -> Result<Digest> {
    let mut json = serde_json::to_value(value)?;
    json.sort_all_objects();
    Ok(Digest::of(&serde_json::to_vec(&json)?))
}
pub(super) fn encrypt(dir: &Path, request: &Request) -> Result<Vec<u8>> {
    let plaintext = Zeroizing::new(serde_json::to_vec(request)?);
    let encoded = protect(dir, &request.binding, b"request", &plaintext)?;
    ensure!(
        encoded.len() as u64 <= MAX_REQUEST,
        "terminal mailbox bytes"
    );
    Ok(encoded)
}
pub(super) fn protect(
    dir: &Path,
    binding: &TerminalBinding,
    purpose: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    let key = key(dir)?;
    let cipher =
        ChaCha20Poly1305::new_from_slice(&key).map_err(|_| anyhow::anyhow!("terminal key"))?;
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let bytes = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &aad(binding, purpose)?,
            },
        )
        .map_err(|_| anyhow::anyhow!("terminal request protection failed"))?;
    let encoded = serde_json::to_vec(&Ciphertext {
        version: 1,
        nonce,
        bytes,
    })?;
    ensure!(
        encoded.len() as u64 <= MAX_LOCAL_FILE,
        "terminal protected bytes"
    );
    Ok(encoded)
}
pub(super) fn decrypt(dir: &Path, binding: &TerminalBinding, encoded: &[u8]) -> Result<Request> {
    let plaintext = unprotect(dir, binding, b"request", encoded)?;
    let request: Request = serde_json::from_slice(&plaintext)
        .map_err(|_| anyhow::anyhow!("invalid terminal request"))?;
    ensure!(
        request.binding == *binding,
        "terminal request binding mismatch"
    );
    request.effect.validate()?;
    ensure!(
        request.effect.terminal() == &binding.terminal,
        "terminal request session mismatch"
    );
    Ok(request)
}
fn unprotect(
    dir: &Path,
    binding: &TerminalBinding,
    purpose: &[u8],
    encoded: &[u8],
) -> Result<Zeroizing<Vec<u8>>> {
    let sealed: Ciphertext = serde_json::from_slice(encoded)
        .map_err(|_| anyhow::anyhow!("invalid protected terminal request"))?;
    ensure!(sealed.version == 1, "terminal request version");
    let key = key(dir)?;
    let cipher =
        ChaCha20Poly1305::new_from_slice(&key).map_err(|_| anyhow::anyhow!("terminal key"))?;
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                Nonce::from_slice(&sealed.nonce),
                Payload {
                    msg: &sealed.bytes,
                    aad: &aad(binding, purpose)?,
                },
            )
            .map_err(|_| anyhow::anyhow!("terminal request authentication failed"))?,
    );
    Ok(plaintext)
}
