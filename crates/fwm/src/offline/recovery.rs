use super::*;
use fwm_core::store::RecoveryReply;

pub(crate) async fn recover(
    paths: &Paths,
    discard_unreadable_intent: bool,
) -> Result<RecoveryReply> {
    if !matches!(
        client::presence(paths).await?,
        client::DaemonPresence::Stopped
    ) {
        bail!("configuration recovery requires a stopped daemon; run `fwm daemon stop` first");
    }
    paths.ensure_dirs()?;
    if std::fs::symlink_metadata(&paths.lock_file).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("refusing symlink daemon lock");
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(&paths.lock_file)?;
    lock.try_lock_exclusive()
        .context("daemon owns this configuration; recovery has not changed any files")?;
    Store::new(paths.clone()).recover_from_candidate(discard_unreadable_intent)
}
