use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::Path,
    process::{self, Command},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const API: &str = "https://api.github.com/repos/nelbin4/Azeroth-Survivors-Trainer/releases/latest";
const DOWNLOADS: &str = "https://github.com/nelbin4/Azeroth-Survivors-Trainer/releases/download";
// Stable Rust releases attach the executable from the Windows x64 Actions artifact.
const EXECUTABLE: &str = "AzerothSurvivorsTrainer.exe";
const MAX_BODY: u64 = 2 * 1024 * 1024;
const MAX_EXECUTABLE: u64 = 64 * 1024 * 1024;

pub enum ReleaseStatus {
    Latest,
    Available(Release),
}

#[derive(Clone)]
pub struct Release {
    tag: String,
    size: u64,
    digest: [u8; 32],
}

fn version(input: &str) -> Option<[u32; 3]> {
    let mut fields = input.strip_prefix('v').unwrap_or(input).split('.');
    let mut numbers = [0; 3];
    for number in &mut numbers {
        let field = fields.next()?;
        if field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        *number = field.parse().ok()?;
    }
    fields.next().is_none().then_some(numbers)
}

pub fn check(installed: &str) -> io::Result<ReleaseStatus> {
    let response = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .redirects(0)
        .build()
        .get(API)
        .set("User-Agent", "AzerothSurvivorsTrainer")
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(io::Error::other)?;
    if response.status() != 200 {
        return Err(io::Error::other("Unexpected GitHub release response."));
    }
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_BODY + 1)
        .read_to_end(&mut body)?;
    if body.len() as u64 > MAX_BODY {
        return Err(io::Error::other("GitHub release response is too large."));
    }
    let release: Value = serde_json::from_slice(&body).map_err(io::Error::other)?;
    release_status(installed, &release)
}

fn release_status(installed: &str, release: &Value) -> io::Result<ReleaseStatus> {
    let (installed_base, development) = installed
        .split_once('-')
        .map_or((installed, false), |(base, _)| (base, true));
    let installed =
        version(installed_base).ok_or_else(|| io::Error::other("Invalid installed version."))?;
    if release["draft"].as_bool() != Some(false) || release["prerelease"].as_bool() != Some(false) {
        return Err(io::Error::other("Expected a stable published release."));
    }
    let tag = release["tag_name"]
        .as_str()
        .ok_or_else(|| io::Error::other("Missing release tag."))?;
    let stable = version(tag).ok_or_else(|| io::Error::other("Invalid release version."))?;
    if stable < installed || (stable == installed && !development) {
        return Ok(ReleaseStatus::Latest);
    }
    let asset = release["assets"]
        .as_array()
        .and_then(|assets| {
            assets
                .iter()
                .find(|asset| asset["name"].as_str() == Some(EXECUTABLE))
        })
        .ok_or_else(|| io::Error::other("The new release is missing its Windows executable."))?;
    let size = asset["size"]
        .as_u64()
        .filter(|size| (1..=MAX_EXECUTABLE).contains(size))
        .ok_or_else(|| io::Error::other("Invalid update size."))?;
    let digest = asset["digest"]
        .as_str()
        .and_then(decode_digest)
        .ok_or_else(|| io::Error::other("The new release is missing its SHA-256 checksum."))?;
    Ok(ReleaseStatus::Available(Release {
        tag: tag.to_owned(),
        size,
        digest,
    }))
}

fn decode_digest(text: &str) -> Option<[u8; 32]> {
    let hex = text.strip_prefix("sha256:")?;
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut digest = [0; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(digest)
}

pub fn install(release: &Release, args: &[OsString]) -> io::Result<()> {
    let url = format!("{DOWNLOADS}/{}/{EXECUTABLE}", release.tag);
    let response = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .redirects(5)
        .build()
        .get(&url)
        .set("User-Agent", "AzerothSurvivorsTrainer")
        .call()
        .map_err(io::Error::other)?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(release.size + 1)
        .read_to_end(&mut bytes)?;
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    if bytes.len() as u64 != release.size || digest != release.digest {
        return Err(io::Error::other(
            "The download did not match the published checksum.",
        ));
    }

    let current = env::current_exe()?;
    let suffix = unique_suffix();
    let staged = current.with_extension(format!("{suffix}.download"));
    let backup = current.with_extension(format!("{suffix}.backup"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)?;
    let result = file.write_all(&bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| {
        replace_and_restart(&current, &staged, &backup, || {
            Command::new(&current).args(args).spawn().map(|_| ())
        })
    });
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

fn unique_suffix() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("update-{}-{}", process::id(), now.as_nanos())
}

fn replace_and_restart(
    current: &Path,
    staged: &Path,
    backup: &Path,
    restart: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    fs::rename(current, backup)?;
    let replacement = fs::rename(staged, current).and_then(|()| restart());
    if let Err(error) = replacement {
        fs::rename(backup, current).map_err(|rollback| {
            io::Error::other(format!(
                "Update failed ({error}). The previous executable is at {} ({rollback}).",
                backup.display()
            ))
        })?;
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_offers_newer_stable_rust_executables_with_checksums() {
        let mut release = json!({
            "tag_name": "v2.1.4", "draft": false, "prerelease": false,
            "assets": []
        });
        assert!(matches!(
            release_status("3.0.0-dev", &release),
            Ok(ReleaseStatus::Latest)
        ));

        release["tag_name"] = json!("v3.0.0");
        assert!(release_status("3.0.0-dev", &release).is_err());
        release["assets"] = json!([{
            "name": EXECUTABLE, "size": 314159,
            "digest": format!("sha256:{}", "a1".repeat(32)),
        }]);
        let Ok(ReleaseStatus::Available(asset)) = release_status("3.0.0-dev", &release) else {
            panic!("expected a verified update asset");
        };
        assert_eq!(asset.digest, [0xa1; 32]);
        assert!(matches!(
            release_status("3.0.0", &release),
            Ok(ReleaseStatus::Latest)
        ));
    }

    #[test]
    fn restores_previous_executable_on_failed_restart() -> io::Result<()> {
        let dir = env::temp_dir().join(unique_suffix());
        fs::create_dir(&dir)?;
        let current = dir.join("trainer.exe");
        let staged = dir.join("trainer.download");
        let backup = dir.join("trainer.backup");
        fs::write(&current, b"old")?;
        fs::write(&staged, b"new")?;
        let error = replace_and_restart(&current, &staged, &backup, || {
            assert_eq!(fs::read(&current)?, b"new");
            Err(io::Error::other("restart failed"))
        })
        .unwrap_err();
        let recovered = fs::read(&current)?;
        fs::remove_file(&current)?;
        fs::remove_dir(&dir)?;
        assert_eq!(error.to_string(), "restart failed");
        assert_eq!(recovered, b"old");
        Ok(())
    }
}
