//! The release contract shared by the app's updater and macOS packaging.

use gpui_kit::{App, AppContext, Entity};
use gpui_updater::{
    CodeSigning, EngineConfig, Error, GitHubSource, Release, UpdateSource, Updater, Verification,
    Version,
};

pub struct KumuReleases;

impl UpdateSource for KumuReleases {
    fn fetch_latest(&self) -> Result<Release, Error> {
        let release = GitHubSource::new("yankewei", "kumu")
            .asset_contains("Kumu-")
            .asset_contains(format!("-{}.dmg", std::env::consts::ARCH))
            .with_checksums("SHA256SUMS")
            .fetch_latest()?;
        // The GitHub source uses substring matching. Never accept a checksum
        // sidecar or an artifact whose version/architecture differs from its tag.
        let expected = format!("Kumu-{}-{}.dmg", release.version, std::env::consts::ARCH);
        if release.asset.name != expected {
            return Err(Error::Parse(format!("expected release asset {expected}")));
        }
        Ok(release)
    }
}

pub fn release_config(current_version: Version) -> EngineConfig {
    EngineConfig::new(current_version)
        .verification(Verification::Checksum)
        .code_signing(CodeSigning::TeamId("3BUD4GCZCQ".to_string()))
}

pub fn init(cx: &mut App) -> Entity<Updater> {
    let config = release_config(
        Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo package version is valid semver"),
    );
    let updater = cx.new(|cx| Updater::new(KumuReleases, config, cx));
    updater.update(cx, |updater, cx| updater.check(cx));
    updater
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_updater::{Asset, UpdateEngine};

    struct ReleaseFixture(Release);

    impl UpdateSource for ReleaseFixture {
        fn fetch_latest(&self) -> Result<Release, Error> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn newer_release_without_checksum_is_not_offered() {
        let release = Release::new(
            Version::new(0, 1, 2),
            Asset::new(
                "Kumu-0.1.2-aarch64.dmg",
                "https://example.com/Kumu.dmg",
                100,
            ),
        );
        let engine = UpdateEngine::new(
            ReleaseFixture(release),
            release_config(Version::new(0, 1, 1)),
        );
        assert!(matches!(
            engine.check(),
            Err(Error::VerificationRequired(_))
        ));
    }
}
