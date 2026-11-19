use serde_json::Value;
use std::{
    io::{self, Read},
    time::Duration,
};

const API: &str = "https://api.github.com/repos/nelbin4/Azeroth-Survivors-Trainer/releases/latest";
const MAX_BODY: u64 = 2 * 1024 * 1024;

pub struct ReleaseStatus {
    pub tag: String,
    pub newer: bool,
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
    let (installed_base, development) = installed
        .split_once('-')
        .map_or((installed, false), |(base, _)| (base, true));
    let installed =
        version(installed_base).ok_or_else(|| io::Error::other("Invalid installed version."))?;
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
    if release["draft"].as_bool() != Some(false) || release["prerelease"].as_bool() != Some(false) {
        return Err(io::Error::other("Expected a stable published release."));
    }
    let tag = release["tag_name"]
        .as_str()
        .ok_or_else(|| io::Error::other("Missing release tag."))?;
    let stable = version(tag).ok_or_else(|| io::Error::other("Invalid release version."))?;
    Ok(ReleaseStatus {
        tag: tag.to_owned(),
        newer: stable > installed || (stable == installed && development),
    })
}
