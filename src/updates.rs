//! The release contract shared by the app's updater and macOS packaging.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use gpui_kit::{App, AppContext, Context, Entity, Global, Task};
pub use gpui_updater_core::{
    CancelToken, CodeSigning, EngineConfig, Error, ErrorKind, GitHubSource, Release, UpdateEngine,
    UpdateSource, UpdateStatus, Verification, Version,
};

struct GlobalUpdater(Entity<Updater>);
impl Global for GlobalUpdater {}

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

type BoxedEngine = UpdateEngine<Box<dyn UpdateSource>>;

/// Observable GPUI entity driving [`gpui_updater_core::UpdateEngine`] on the
/// background executor without coupling Kumu to `gpui-updater-pre` (`gpui-pre`).
pub struct Updater {
    status: UpdateStatus,
    available: Option<Release>,
    engine: Arc<BoxedEngine>,
    task: Option<Task<()>>,
    cancel: Option<CancelToken>,
}

impl Updater {
    pub fn new<S: UpdateSource>(source: S, config: EngineConfig) -> Self {
        let engine = UpdateEngine::new(Box::new(source) as Box<dyn UpdateSource>, config);
        Self {
            status: UpdateStatus::Idle,
            available: None,
            engine: Arc::new(engine),
            task: None,
            cancel: None,
        }
    }

    pub fn status(&self) -> &UpdateStatus {
        &self.status
    }

    pub fn available(&self) -> Option<&Release> {
        self.available.as_ref()
    }

    fn set_status(&mut self, status: UpdateStatus, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    pub fn check(&mut self, cx: &mut Context<Self>) {
        if self.status.is_busy() {
            return;
        }
        let engine = Arc::clone(&self.engine);
        self.set_status(UpdateStatus::Checking, cx);
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { engine.check() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.task = None;
                match result {
                    Ok(Some(release)) => {
                        let version = release.version.clone();
                        this.available = Some(release);
                        this.set_status(UpdateStatus::Available(version), cx);
                    }
                    Ok(None) => this.set_status(UpdateStatus::UpToDate, cx),
                    Err(e) => this.set_status(UpdateStatus::Errored(e.into()), cx),
                }
            });
        }));
    }

    pub fn download_and_install(&mut self, cx: &mut Context<Self>) {
        if self.status.is_busy() {
            return;
        }
        let Some(release) = self.available.clone() else {
            return;
        };
        let engine = Arc::clone(&self.engine);
        let cancel = CancelToken::new();
        self.cancel = Some(cancel.clone());
        self.set_status(
            UpdateStatus::Downloading {
                downloaded: 0,
                total: None,
            },
            cx,
        );
        self.task = Some(cx.spawn(async move |this, cx| {
            let downloaded = Arc::new(AtomicU64::new(0));
            let total = Arc::new(AtomicU64::new(0));
            let done = Arc::new(AtomicBool::new(false));

            let dl_task = {
                let (engine, release) = (Arc::clone(&engine), release.clone());
                let (d, t, fin) = (
                    Arc::clone(&downloaded),
                    Arc::clone(&total),
                    Arc::clone(&done),
                );
                cx.background_executor().spawn(async move {
                    let result = engine.download_cancellable(&release, &cancel, |got, tot| {
                        d.store(got, Ordering::Relaxed);
                        t.store(tot.unwrap_or(0), Ordering::Relaxed);
                    });
                    fin.store(true, Ordering::Relaxed);
                    result
                })
            };

            loop {
                let got = downloaded.load(Ordering::Relaxed);
                let tot = total.load(Ordering::Relaxed);
                let _ = this.update(cx, |this, cx| {
                    this.set_status(
                        UpdateStatus::Downloading {
                            downloaded: got,
                            total: (tot != 0).then_some(tot),
                        },
                        cx,
                    );
                });
                if done.load(Ordering::Relaxed) {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
            }

            let artifact = match dl_task.await {
                Ok(artifact) => artifact,
                Err(e) => {
                    let _ = this.update(cx, |this, cx| {
                        this.task = None;
                        this.cancel = None;
                        let next = if e.kind() == ErrorKind::Cancelled {
                            UpdateStatus::Available(release.version.clone())
                        } else {
                            UpdateStatus::Errored(e.into())
                        };
                        this.set_status(next, cx);
                    });
                    return;
                }
            };

            let _ = this.update(cx, |this, cx| {
                this.cancel = None;
                this.set_status(UpdateStatus::Installing, cx);
            });

            let installed = {
                let engine = Arc::clone(&engine);
                cx.background_executor()
                    .spawn(async move { engine.install(&artifact) })
                    .await
            };
            let _ = this.update(cx, |this, cx| {
                this.task = None;
                match installed {
                    Ok(installed) => {
                        if let Some(path) = &installed.restart_path {
                            cx.set_restart_path(path.clone());
                        }
                        let version = release.version.clone();
                        this.set_status(UpdateStatus::Staged(version), cx);
                    }
                    Err(e) => this.set_status(UpdateStatus::Errored(e.into()), cx),
                }
            });
        }));
    }

    pub fn cancel(&mut self, _cx: &mut Context<Self>) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
    }

    pub fn restart(&mut self, cx: &mut Context<Self>) {
        cx.restart();
    }
}

impl Drop for Updater {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
    }
}

pub fn init(cx: &mut App) -> Entity<Updater> {
    if let Some(existing) = cx.try_global::<GlobalUpdater>() {
        return existing.0.clone();
    }
    let config = release_config(
        Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo package version is valid semver"),
    );
    let updater = cx.new(|_| Updater::new(KumuReleases, config));
    updater.update(cx, |updater, cx| updater.check(cx));
    cx.set_global(GlobalUpdater(updater.clone()));
    updater
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_updater_core::{Asset, UpdateEngine};

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
