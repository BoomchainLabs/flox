use std::path::PathBuf;
use std::sync::LazyLock;

use catalog_api_v1::types::{MessageLevel, PackageSystem};
#[cfg(test)]
use flox_test_utils::proptest::{alphanum_string, chrono_strat};
use indent::{indent_all_by, indent_by};
use indoc::formatdoc;
use itertools::{Either, Itertools};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_with::skip_serializing_none;
use tracing::instrument;

pub type FlakeRef = Value;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Display;
use std::fs;
use std::str::FromStr;

use flox_core::Version;
use thiserror::Error;
use tracing::debug;

use super::environment::fetcher::IncludeFetcher;
use super::environment::{CoreEnvironmentError, EnvironmentError};
use super::manifest::composite::{ManifestMerger, MergeError, ShallowMerger, WarningWithContext};
use super::manifest::typed::{
    Allows,
    DEFAULT_GROUP_NAME,
    DEFAULT_PRIORITY,
    IncludeDescriptor,
    Inner,
    Manifest,
    ManifestError,
    ManifestPackageDescriptor,
    PackageDescriptorCatalog,
    PackageDescriptorFlake,
};
use crate::data::{CanonicalPath, System};
use crate::flox::Flox;
use crate::models::manifest::composite::CompositeManifest;
use crate::providers::catalog::{
    self,
    ALL_SYSTEMS,
    CatalogPage,
    MsgAttrPathNotFoundNotFoundForAllSystems,
    MsgAttrPathNotFoundNotInCatalog,
    MsgAttrPathNotFoundSystemsNotOnSamePage,
    MsgConstraintsTooTight,
    MsgUnknown,
    PackageDescriptor,
    PackageGroup,
    ResolvedPackageGroup,
};
use crate::providers::flake_installable_locker::{
    FlakeInstallableError,
    InstallableLocker,
    LockedInstallable,
};

pub(crate) static DEFAULT_SYSTEMS_STR: LazyLock<[String; 4]> =
    LazyLock::new(|| ALL_SYSTEMS.map(|system| system.to_string()));

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Input {
    pub from: FlakeRef,
    #[serde(flatten)]
    _json: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Registry {
    pub inputs: BTreeMap<String, Input>,
    #[serde(flatten)]
    _json: Value,
}

#[derive(Debug, Clone)]
pub enum LockResult {
    /// Locking produced a new Lockfile.
    /// The change could be a minimal as whitespace.
    Changed(Lockfile),
    /// Locking did not produce a new Lockfile.
    Unchanged(Lockfile),
}

impl From<LockResult> for Lockfile {
    fn from(result: LockResult) -> Self {
        match result {
            LockResult::Changed(lockfile) => lockfile,
            LockResult::Unchanged(lockfile) => lockfile,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub struct Lockfile {
    #[serde(rename = "lockfile-version")]
    pub version: Version<1>,
    /// The manifest that was locked.
    ///
    /// For an environment that doesn't include any others, this is the `manifest.toml`
    /// on disk at lock-time. For an environment that *does* include others, this is
    /// the merged manifest that was locked.
    pub manifest: Manifest,
    /// Locked packages
    pub packages: Vec<LockedPackage>,
    /// Composition information. This will be `None` when there are no includes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose: Option<Compose>, // use `is_none()` to detect composition
}

impl Lockfile {
    pub fn read_from_file(path: &CanonicalPath) -> Result<Self, CoreEnvironmentError> {
        let contents = fs::read(path).map_err(CoreEnvironmentError::ReadLockfile)?;
        serde_json::from_slice(&contents).map_err(CoreEnvironmentError::ParseLockfile)
    }

    pub fn version(&self) -> u8 {
        1
    }
}

impl FromStr for Lockfile {
    type Err = CoreEnvironmentError;

    fn from_str(contents: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(contents).map_err(CoreEnvironmentError::ParseLockfile)
    }
}

impl Display for Lockfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", serde_json::json!(self))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, derive_more::From)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
#[serde(untagged)]
pub enum LockedPackage {
    Catalog(LockedPackageCatalog),
    Flake(LockedPackageFlake),
    StorePath(LockedPackageStorePath),
}

impl LockedPackage {
    pub(crate) fn as_catalog_package_ref(&self) -> Option<&LockedPackageCatalog> {
        match self {
            LockedPackage::Catalog(pkg) => Some(pkg),
            _ => None,
        }
    }

    pub fn install_id(&self) -> &str {
        match self {
            LockedPackage::Catalog(pkg) => &pkg.install_id,
            LockedPackage::Flake(pkg) => &pkg.install_id,
            LockedPackage::StorePath(pkg) => &pkg.install_id,
        }
    }

    pub(crate) fn system(&self) -> &System {
        match self {
            LockedPackage::Catalog(pkg) => &pkg.system,
            LockedPackage::Flake(pkg) => &pkg.locked_installable.system,
            LockedPackage::StorePath(pkg) => &pkg.system,
        }
    }

    pub fn broken(&self) -> Option<bool> {
        match self {
            LockedPackage::Catalog(pkg) => pkg.broken,
            LockedPackage::Flake(pkg) => pkg.locked_installable.broken,
            LockedPackage::StorePath(_) => None,
        }
    }

    pub fn unfree(&self) -> Option<bool> {
        match self {
            LockedPackage::Catalog(pkg) => pkg.unfree,
            LockedPackage::Flake(pkg) => pkg.locked_installable.unfree,
            LockedPackage::StorePath(_) => None,
        }
    }

    pub fn derivation(&self) -> Option<&str> {
        match self {
            LockedPackage::Catalog(pkg) => Some(&pkg.derivation),
            LockedPackage::Flake(pkg) => Some(&pkg.locked_installable.derivation),
            // Technically store paths _may_ have a derivation,
            // but it's not quite relevant yet for us to record it in the lockfile.
            LockedPackage::StorePath(_) => None,
        }
    }

    pub fn version(&self) -> Option<&str> {
        match self {
            LockedPackage::Catalog(pkg) => Some(&pkg.version),
            LockedPackage::Flake(pkg) => pkg.locked_installable.version.as_deref(),
            LockedPackage::StorePath(_) => None,
        }
    }
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub struct LockedPackageCatalog {
    // region: original fields from the service
    // These fields are copied from the generated struct.
    pub attr_path: String,
    pub broken: Option<bool>,
    pub derivation: String,
    pub description: Option<String>,
    pub install_id: String,
    pub license: Option<String>,
    pub locked_url: String,
    pub name: String,
    pub pname: String,
    pub rev: String,
    pub rev_count: i64,
    #[cfg_attr(test, proptest(strategy = "chrono_strat()"))]
    pub rev_date: chrono::DateTime<chrono::offset::Utc>,
    #[cfg_attr(test, proptest(strategy = "chrono_strat()"))]
    pub scrape_date: chrono::DateTime<chrono::offset::Utc>,
    pub stabilities: Option<Vec<String>>,
    pub unfree: Option<bool>,
    pub version: String,
    pub outputs_to_install: Option<Vec<String>>,
    // endregion

    // region: converted fields
    pub outputs: BTreeMap<String, String>,
    // endregion

    // region: added fields
    pub system: System, // FIXME: this is an enum in the generated code, can't derive Arbitrary there
    pub group: String,
    // This was previously a `usize`, but in Nix `priority` is a `NixInt`, which is explicitly
    // a `uint64_t` instead of a `size_t`. Using a `u64` here matches those semantics, though in
    // reality it's likely not an issue.
    pub priority: u64,
    // endregion
}

impl LockedPackageCatalog {
    /// Construct a [LockedPackageCatalog] from a [ManifestPackageDescriptor],
    /// the resolved [catalog::PackageResolutionInfo], and corresponding [System].
    ///
    /// There may be more validation/parsing we could do here in the future.
    pub fn from_parts(
        package: catalog::PackageResolutionInfo,
        descriptor: PackageDescriptorCatalog,
    ) -> Self {
        // unpack package to avoid missing new fields
        let catalog::PackageResolutionInfo {
            catalog: _,
            attr_path,
            broken,
            derivation,
            description,
            // TODO: we should add this to LockedPackageCatalog
            insecure: _,
            install_id,
            license,
            locked_url,
            name,
            outputs,
            outputs_to_install,
            pname,
            rev,
            rev_count,
            rev_date,
            scrape_date,
            stabilities,
            unfree,
            version,
            system,
            cache_uri: _,
            pkg_path: _,
            missing_builds: _,
        } = package;

        let outputs = outputs
            .iter()
            .map(|output| (output.name.clone(), output.store_path.clone()))
            .collect::<BTreeMap<_, _>>();

        let priority = descriptor.priority.unwrap_or(DEFAULT_PRIORITY);
        let group = descriptor
            .pkg_group
            .as_deref()
            .unwrap_or(DEFAULT_GROUP_NAME)
            .to_string();

        LockedPackageCatalog {
            attr_path,
            broken,
            derivation,
            description,
            install_id,
            license,
            locked_url,
            name,
            outputs,
            outputs_to_install,
            pname,
            rev,
            rev_count,
            rev_date,
            // This field is deprecated and should be removed in the future,
            // currently it should always be populated, but if it's not, we can
            // default since it's not relied upon for anything downstream.
            scrape_date: scrape_date.unwrap_or(chrono::Utc::now()),
            stabilities,
            unfree,
            version,
            system: system.to_string(),
            priority,
            group,
        }
    }
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub struct LockedPackageFlake {
    pub install_id: String,
    /// Unaltered lock information as returned by `lock-flake-installable`.
    /// In this case we completely own the data format in this repo
    /// and so far have to do no conversion.
    /// If this changes in the future, we can add a conversion layer here
    /// similar to [LockedPackageCatalog::from_parts].
    #[serde(flatten)]
    pub locked_installable: LockedInstallable,
}

impl LockedPackageFlake {
    /// Construct a [LockedPackageFlake] from an [LockedInstallable] and an install_id.
    /// In the future, we may want to pass the original descriptor here as well,
    /// similar to [LockedPackageCatalog::from_parts].
    pub fn from_parts(install_id: String, locked_installable: LockedInstallable) -> Self {
        LockedPackageFlake {
            install_id,
            locked_installable,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct FlakeInstallableToLock {
    install_id: String,
    descriptor: PackageDescriptorFlake,
    system: System,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[skip_serializing_none]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub struct LockedPackageStorePath {
    /// The install_id of the descriptor in the manifest
    pub install_id: String,
    /// Store path to add to the environment
    pub store_path: String,
    pub system: System,
    pub priority: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LockedGroup {
    /// name of the group
    name: String,
    /// system this group provides packages for
    system: System,
    /// [CatalogPage] that was selected to fulfill this group
    ///
    /// If resolution of a group provides multiple pages,
    /// a single page is selected based on cross group constraints.
    /// By default this is the latest page that provides packages
    /// for all requested systems.
    page: CatalogPage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub struct Compose {
    /// The composing environment's manifest that was on disk at lock-time.
    pub composer: Manifest,
    /// Metadata and manifests for the included environments in the order
    /// that they were specified in the composing environment's manifest.
    pub include: Vec<LockedInclude>,
    /// Warnings generated during composition + locking.
    pub warnings: Vec<WarningWithContext>,
}

impl Compose {
    /// Get the highest priority included environment which provides each package.
    /// Packages that are not provided by any included environments will be absent from the map.
    pub fn get_includes_for_packages(
        &self,
        packages: &[String],
    ) -> Result<HashMap<String, LockedInclude>, ManifestError> {
        let mut result = HashMap::new();
        for package in packages {
            if let Some(include) = Self::get_include_for_package(package, &self.include)? {
                result.insert(package.clone(), include);
            }
        }

        Ok(result)
    }

    /// Detect which included environment, if any, provides a given package.
    fn get_include_for_package(
        package: &String,
        includes: &[LockedInclude],
    ) -> Result<Option<LockedInclude>, ManifestError> {
        // Reverse of merge order so that we return the highest priority match.
        for include in includes.iter().rev() {
            match include.manifest.get_install_ids(vec![package.to_string()]) {
                Ok(_) => return Ok(Some(include.clone())),
                Err(ManifestError::PackageNotFound(_)) => continue,
                Err(ManifestError::MultiplePackagesMatch(_, _)) => continue,
                Err(err) => return Err(err),
            }
        }

        Ok(None)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub struct LockedInclude {
    pub manifest: Manifest,
    #[cfg_attr(test, proptest(strategy = "alphanum_string(5)"))]
    pub name: String,
    pub descriptor: IncludeDescriptor,
    // TODO: Record generation if/when:
    // 1. We have a need for it in presentation, e.g.
    //   - https://github.com/flox/flox/issues/2948
    // 2. Generations work has settled:
    //   - https://github.com/flox/product/pull/881
    //   - https://github.com/flox/product/pull/891
    // 3. We've exposed it from `RemoteEnvironment`/`ManagedEnvironment`
    // pub remote: Option<Generation>,
}

/// All the resolution failures for a single resolution request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolutionFailures(pub Vec<ResolutionFailure>);

impl FromIterator<ResolutionFailure> for ResolutionFailures {
    fn from_iter<T: IntoIterator<Item = ResolutionFailure>>(iter: T) -> Self {
        ResolutionFailures(iter.into_iter().collect())
    }
}

/// Data relevant for formatting a resolution failure
///
/// This may wrap messages returned from the catalog with additional information
/// extracted from the manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ResolutionFailure {
    PackageNotFound(MsgAttrPathNotFoundNotInCatalog),
    PackageUnavailableOnSomeSystems {
        catalog_message: MsgAttrPathNotFoundNotFoundForAllSystems,
        invalid_systems: Vec<String>,
    },
    SystemsNotOnSamePage(MsgAttrPathNotFoundSystemsNotOnSamePage),
    ConstraintsTooTight {
        catalog_message: MsgConstraintsTooTight,
        group: String,
    },
    UnknownServiceMessage(MsgUnknown),
    FallbackMessage {
        msg: String,
    },
}

// Convenience for when you just have a single message
impl From<ResolutionFailure> for ResolutionFailures {
    fn from(value: ResolutionFailure) -> Self {
        ResolutionFailures::from_iter([value])
    }
}

impl Display for ResolutionFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let formatted = if self.0.len() > 1 {
            format_multiple_resolution_failures(&self.0)
        } else {
            format_single_resolution_failure(&self.0[0], false)
        };
        write!(f, "{formatted}")
    }
}

/// Formats a single resolution failure in a nice way
fn format_single_resolution_failure(failure: &ResolutionFailure, is_one_of_many: bool) -> String {
    match failure {
        ResolutionFailure::PackageNotFound(MsgAttrPathNotFoundNotInCatalog {
            attr_path, ..
        }) => {
            // Note: for `flox install`, this variant will be formatted with the
            // "didyoumean" mechanism.
            format!("could not find package '{attr_path}'.")
        },
        ResolutionFailure::PackageUnavailableOnSomeSystems {
            catalog_message:
                MsgAttrPathNotFoundNotFoundForAllSystems {
                    attr_path,
                    valid_systems,
                    ..
                },
            invalid_systems,
            ..
        } => {
            let extra_indent = if is_one_of_many { 2 } else { 0 };
            let indented_invalid = invalid_systems
                .iter()
                .sorted()
                .map(|s| indent_all_by(4, format!("- {s}")))
                .join("\n");
            let indented_valid = valid_systems
                .iter()
                .sorted()
                .map(|s| indent_all_by(4, format!("- {s}")))
                .join("\n");
            let listed = [
                format!("package '{attr_path}' not available for"),
                indented_invalid,
                indent_all_by(2, "but it is available for"),
                indented_valid,
            ]
            .join("\n");
            let with_doc_link = formatdoc! {"
            {listed}

            For more on managing system-specific packages, visit the documentation:
            https://flox.dev/docs/tutorials/multi-arch-environments/#handling-unsupported-packages"};
            indent_by(extra_indent, with_doc_link)
        },
        ResolutionFailure::ConstraintsTooTight { group, .. } => {
            let extra_indent = if is_one_of_many { 2 } else { 3 };
            let base_msg = format!("constraints for group '{group}' are too tight");
            let msg = formatdoc! {"
            {base_msg}

            Use 'flox edit' to adjust version constraints in the [install] section,
            or isolate dependencies in a new group with '<pkg>.pkg-group = \"newgroup\"'"};
            indent_by(extra_indent, msg)
        },
        ResolutionFailure::SystemsNotOnSamePage(MsgAttrPathNotFoundSystemsNotOnSamePage {
            msg,
            ..
        })
        | ResolutionFailure::UnknownServiceMessage(MsgUnknown { msg, .. })
        | ResolutionFailure::FallbackMessage { msg } => {
            if is_one_of_many {
                indent_by(2, msg.to_string())
            } else {
                format!("\n{}", msg)
            }
        },
    }
}

/// Formats several resolution messages in a more legible way than just one per line
fn format_multiple_resolution_failures(failures: &[ResolutionFailure]) -> String {
    let msgs = failures
        .iter()
        .map(|f| format!("- {}", format_single_resolution_failure(f, true)))
        .collect::<Vec<_>>()
        .join("\n");
    format!("multiple resolution failures:\n{msgs}")
}

impl Lockfile {
    /// Convert a locked manifest to a list of installed packages for a given system.
    pub fn list_packages(&self, system: &System) -> Result<Vec<PackageToList>, ResolveError> {
        self.packages
            .iter()
            .filter(|package| package.system() == system)
            .cloned()
            .map(|package| match package {
                LockedPackage::Catalog(pkg) => {
                    let descriptor = self
                        .manifest
                        .pkg_descriptor_with_id(&pkg.install_id)
                        .ok_or(ResolveError::MissingPackageDescriptor(
                            pkg.install_id.clone(),
                        ))?;

                    let Some(descriptor) = descriptor.unwrap_catalog_descriptor() else {
                        Err(ResolveError::MissingPackageDescriptor(
                            pkg.install_id.clone(),
                        ))?
                    };

                    Ok(PackageToList::Catalog(descriptor, pkg))
                },
                LockedPackage::Flake(locked_package) => {
                    let descriptor = self
                        .manifest
                        .pkg_descriptor_with_id(&locked_package.install_id)
                        .ok_or(ResolveError::MissingPackageDescriptor(
                            locked_package.install_id.clone(),
                        ))?;

                    let Some(descriptor) = descriptor.unwrap_flake_descriptor() else {
                        Err(ResolveError::MissingPackageDescriptor(
                            locked_package.install_id.clone(),
                        ))?
                    };

                    Ok(PackageToList::Flake(descriptor, locked_package))
                },
                LockedPackage::StorePath(locked) => Ok(PackageToList::StorePath(locked)),
            })
            .collect::<Result<Vec<_>, ResolveError>>()
    }

    /// Merge included environments, resolve the merged manifest, and return the resulting lockfile
    ///
    /// Already resolved packages will not be re-resolved,
    /// and already fetched includes will not be re-fetched.
    pub async fn lock_manifest(
        flox: &Flox,
        manifest: &Manifest,
        seed_lockfile: Option<&Lockfile>,
        include_fetcher: &IncludeFetcher,
    ) -> Result<Lockfile, EnvironmentError> {
        Self::lock_manifest_with_include_upgrades(
            flox,
            manifest,
            seed_lockfile,
            include_fetcher,
            None,
        )
        .await
    }

    /// Lock, upgrading the specified included environments if to_upgrade is
    /// Some
    ///
    /// If to_upgrade is an empty vector, all included environments are
    /// re-fetched.
    /// If to_upgrade is None, only included environments not in the seed lockfile
    /// are fetched.
    pub async fn lock_manifest_with_include_upgrades(
        flox: &Flox,
        manifest: &Manifest,
        seed_lockfile: Option<&Lockfile>,
        include_fetcher: &IncludeFetcher,
        to_upgrade: Option<Vec<String>>,
    ) -> Result<Lockfile, EnvironmentError> {
        let (merged, compose) = Self::merge_manifest(
            flox,
            manifest,
            seed_lockfile,
            include_fetcher,
            ManifestMerger::Shallow(ShallowMerger),
            to_upgrade,
        )
        .map_err(EnvironmentError::Recoverable)?;
        let packages = Self::resolve_manifest(
            &merged,
            seed_lockfile,
            &flox.catalog_client,
            &flox.installable_locker,
        )
        .await
        .map_err(|e| EnvironmentError::Core(CoreEnvironmentError::Resolve(e)))?;
        let lockfile = Lockfile {
            version: Version::<1>,
            manifest: merged,
            packages,
            compose,
        };

        Ok(lockfile)
    }

    /// Fetch included environments and merge them with the manifest, returning
    /// the merged manifest and a Compose object with the contents of all fetched includes.
    ///
    /// If the manifest does not include any environments, None is returned
    /// instead of a Compose object.
    ///
    /// Any included environments already in the seed lockfile will not be
    /// re-fetched, unless they are in to_upgrade.
    ///
    /// All environments in to_upgrade will be re-fetched, and an error is
    /// returned if any environments in to_upgrade do not refer to an actual include.
    ///
    /// If to_upgrade is an empty vector, all included environments are
    /// re-fetched.
    /// If to_upgrade is None, only included environments not in the seed lockfile
    /// are fetched.
    #[instrument(skip_all, fields(progress = "Composing environments"))]
    fn merge_manifest(
        flox: &Flox,
        manifest: &Manifest,
        seed_lockfile: Option<&Lockfile>,
        include_fetcher: &IncludeFetcher,
        merger: ManifestMerger,
        mut to_upgrade: Option<Vec<String>>,
    ) -> Result<(Manifest, Option<Compose>), RecoverableMergeError> {
        if manifest.include.environments.is_empty() {
            if to_upgrade.is_some() {
                return Err(RecoverableMergeError::Catchall(
                    "environment has no included environments".to_string(),
                ));
            }
            return Ok((manifest.clone(), None));
        }

        debug!("composing included environments");

        // Fetch included manifests we don't already have in seed_lockfile.
        // Note that we have to preserve the order of the includes in the
        // manifest.
        let mut locked_includes: Vec<LockedInclude> = vec![];
        let upgrade_all = to_upgrade
            .as_ref()
            .map(|to_upgrade| to_upgrade.is_empty())
            .unwrap_or(false);
        for include_environment in &manifest.include.environments {
            debug!(
                name = include_environment.to_string(),
                "inspecting included environment"
            );
            let existing_locked_include = 'existing: {
                // Don't use existing locked includes if we're upgradeing all
                // includes
                if upgrade_all {
                    break 'existing None;
                }

                // If there's a seed_lockfile
                let Some(seed_lockfile) = seed_lockfile else {
                    break 'existing None;
                };
                // And the seed lockfile was generated from a manifest with includes
                let Some(compose) = &seed_lockfile.compose else {
                    break 'existing None;
                };
                // And we can find an identical include descriptor in the seed lockfile
                // Then use the existing locked include
                compose
                    .include
                    .iter()
                    .find(|locked_include| &locked_include.descriptor == include_environment)
                    .cloned()
            };

            let locked_include = match existing_locked_include {
                Some(locked_include) => {
                    debug!("found existing locked include for {include_environment}");
                    // The following is a weird edge case,
                    // but I don't think it's too much of a problem:
                    // Suppose composer includes ./dir1 which has name A in
                    // env.json
                    // ./dir1 gets renamed A -> B
                    // A manual edit includes ./dir2 which has name A in
                    // env.json
                    // If we upgrade name A, if we loop over ./dir1 first, we'll
                    // fetch both ./dir1 and ./dir2.
                    // If we loop over ./dir2 first, we'll only fetch ./dir2.

                    // Check if the existing locked include needs to be upgraded
                    // If it does, remove it from to_upgrade to keep track of
                    // which includes have been upgraded.
                    let should_refetch = to_upgrade
                        .as_mut()
                        .map(|to_upgrade| {
                            Self::remove_matching_include(to_upgrade, &locked_include)
                        })
                        .unwrap_or(false);

                    if should_refetch {
                        debug!(
                            name = include_environment.to_string(),
                            "upgrading included environment"
                        );
                        include_fetcher
                            .fetch(flox, include_environment)
                            .map_err(|e| RecoverableMergeError::Fetch {
                                include: include_environment.clone(),
                                err: Box::new(e),
                            })?
                    } else {
                        debug!(
                            name = include_environment.to_string(),
                            "using existing locked include from lockfile"
                        );

                        locked_include
                    }
                },
                None => {
                    debug!(
                        name = include_environment.to_string(),
                        "fetching included environment"
                    );

                    let locked_include =
                        include_fetcher
                            .fetch(flox, include_environment)
                            .map_err(|e| RecoverableMergeError::Fetch {
                                include: include_environment.clone(),
                                err: Box::new(e),
                            })?;
                    // If this include needed to be upgraded, remove from
                    // to_upgrade to keep track that it was
                    if let Some(to_upgrade) = &mut to_upgrade {
                        Self::remove_matching_include(to_upgrade, &locked_include);
                    }
                    locked_include
                },
            };
            locked_includes.push(locked_include);
        }

        Self::check_locked_names_unique(&locked_includes)?;

        if let Some(to_upgrade) = &to_upgrade {
            if let Some(unused_include_to_upgrade) = to_upgrade.first() {
                return Err(RecoverableMergeError::Catchall(format!(
                    "unknown included environment to check for changes '{}'",
                    unused_include_to_upgrade
                )));
            }
        }

        // Call the merger with all the manifests
        let composite = CompositeManifest {
            composer: manifest.clone(),
            deps: locked_includes
                .iter()
                .map(|include| (include.name.clone(), include.manifest.clone()))
                .collect(),
        };

        let (merged, warnings) = composite
            .merge_all(merger)
            .map_err(RecoverableMergeError::Merge)?;

        // Stitch everything together into a Compose object
        let compose = Compose {
            composer: manifest.clone(),
            include: locked_includes,
            warnings,
        };

        Ok((merged, Some(compose)))
    }

    /// Helper method that removes the first IncludeToUpgrade that matches a given
    /// LockedInclude.
    /// Used to keep track of what includes have been upgraded.
    fn remove_matching_include(
        to_upgrade: &mut Vec<String>,
        locked_include: &LockedInclude,
    ) -> bool {
        let position = to_upgrade
            .iter()
            .position(|name| &locked_include.name == name);
        match position {
            Some(position) => {
                to_upgrade.swap_remove(position);
                true
            },
            None => false,
        }
    }

    /// Check that all names in a list of locked includes are unique
    fn check_locked_names_unique(
        locked_includes: &[LockedInclude],
    ) -> Result<(), RecoverableMergeError> {
        let mut seen_names = HashSet::new();
        for locked_include in locked_includes {
            if !seen_names.insert(&locked_include.name) {
                return Err(RecoverableMergeError::Catchall(formatdoc! {
                "multiple environments in include.environments have the name '{}'
                 A unique name can be provided with the 'name' field.", locked_include.name}));
            }
        }
        Ok(())
    }

    /// Resolve packages for a given manifest
    ///
    /// Uses the catalog service to resolve [ManifestPackageDescriptorCatalog],
    /// and an [InstallableLocker] to lock [ManifestPackageDescriptorFlake] descriptors.
    ///
    /// If a seed lockfile is provided, packages that are already locked
    /// will constrain the resolution of catalog packages to the same derivation.
    /// Already locked flake installables will not be locked again,
    /// and copied from the seed lockfile as is.
    ///
    /// Catalog and flake installables are locked separately, using largely symmetric logic.
    /// Keeping the locking of each kind separate keeps the existing methods simpler
    /// and allows for potential parallelization in the future.
    #[instrument(skip_all, fields(progress = "Locking environment"))]
    async fn resolve_manifest(
        manifest: &Manifest,
        seed_lockfile: Option<&Lockfile>,
        client: &impl catalog::ClientTrait,
        installable_locker: &impl InstallableLocker,
    ) -> Result<Vec<LockedPackage>, ResolveError> {
        let catalog_groups = Self::collect_package_groups(manifest, seed_lockfile)?;
        let (mut already_locked_packages, groups_to_lock) =
            Self::split_fully_locked_groups(catalog_groups, seed_lockfile);

        let flake_installables = Self::collect_flake_installables(manifest);
        let (already_locked_installables, installables_to_lock) =
            Self::split_locked_flake_installables(flake_installables, seed_lockfile);

        // Store paths are locked by definition
        let locked_store_paths = Self::collect_store_paths(manifest)
            .into_iter()
            .map(LockedPackage::StorePath)
            .collect();

        // The manifest could have been edited since locking packages,
        // in which case there may be packages that aren't allowed.
        Self::check_packages_are_allowed(
            already_locked_packages
                .iter()
                .filter_map(LockedPackage::as_catalog_package_ref),
            &manifest.options.allow,
        )?;

        // Update the priority of already locked packages to match the manifest.
        Self::update_priority(&mut already_locked_packages, manifest);

        if groups_to_lock.is_empty() && installables_to_lock.is_empty() {
            debug!("All packages are already locked, skipping resolution");
            return Ok([
                locked_store_paths,
                already_locked_packages,
                already_locked_installables,
            ]
            .concat());
        }

        // lock packages
        let resolved = if !groups_to_lock.is_empty() {
            client
                .resolve(groups_to_lock)
                .await
                .map_err(ResolveError::CatalogResolve)?
        } else {
            vec![]
        };

        // unpack locked packages from response
        let locked_packages: Vec<LockedPackage> =
            Self::locked_packages_from_resolution(manifest, resolved)?
                .map(Into::into)
                .collect();

        let locked_installables = if !installables_to_lock.is_empty() {
            Self::lock_flake_installables(installable_locker, installables_to_lock)?
                .map(Into::into)
                .collect()
        } else {
            vec![]
        };

        // The server should be checking this,
        // but double check
        Self::check_packages_are_allowed(
            locked_packages
                .iter()
                .filter_map(LockedPackage::as_catalog_package_ref),
            &manifest.options.allow,
        )?;

        Ok([
            locked_store_paths,
            already_locked_packages,
            locked_packages,
            already_locked_installables,
            locked_installables,
        ]
        .concat())
    }

    /// Given locked packages and manifest options allows, verify that the
    /// locked packages are allowed.
    fn check_packages_are_allowed<'a>(
        locked_packages: impl IntoIterator<Item = &'a LockedPackageCatalog>,
        allow: &Allows,
    ) -> Result<(), ResolveError> {
        for package in locked_packages {
            if let Some(ref licenses) = allow.licenses {
                // If licenses is empty, allow any license.
                // There isn't any reason to disallow all licenses,
                // and setting licenses to [] is the only way with composition
                // currently to allow all licenses if an included environment has licenses.
                if !licenses.is_empty() {
                    let Some(ref license) = package.license else {
                        continue;
                    };

                    if !licenses.iter().any(|allowed| allowed == license) {
                        return Err(ResolveError::LicenseNotAllowed(
                            package.install_id.to_string(),
                            license.to_string(),
                        ));
                    }
                }
            }

            // Don't allow broken by default
            if !allow.broken.unwrap_or(false) {
                // Assume a package isn't broken
                if package.broken.unwrap_or(false) {
                    return Err(ResolveError::BrokenNotAllowed(
                        package.install_id.to_owned(),
                    ));
                }
            }

            // Allow unfree by default
            if !allow.unfree.unwrap_or(true) {
                // Assume a package isn't unfree
                if package.unfree.unwrap_or(false) {
                    return Err(ResolveError::UnfreeNotAllowed(
                        package.install_id.to_owned(),
                    ));
                }
            }
        }

        Ok(())
    }

    /// Update the priority of already locked packages to match the manifest.
    ///
    /// The `priority` field is originally set when constructing in [LockedPackageCatalog::from_parts],
    /// after resolution.
    /// Already locked packages are not re-resolved for priority changes
    /// as priority is not a constraint for resolution.
    /// The priority in the manifest may have changed since the package was locked,
    /// so we update the priority of already locked packages to match the manifest.
    fn update_priority<'a>(
        already_locked_packages: impl IntoIterator<Item = &'a mut LockedPackage>,
        manifest: &Manifest,
    ) {
        for locked_package in already_locked_packages {
            let LockedPackage::Catalog(LockedPackageCatalog {
                install_id,
                priority,
                ..
            }) = locked_package
            else {
                // `already_locked_packages`` should only contain catalog packages to begin with
                // and locked flake installables do not have a priority (yet?),
                // so this shouldn't occur.
                return;
            };

            let new_priority = manifest
                .install
                .inner()
                .get(install_id)
                .and_then(|descriptor| descriptor.as_catalog_descriptor_ref())
                .and_then(|descriptor| descriptor.priority)
                .unwrap_or(DEFAULT_PRIORITY);

            *priority = new_priority;
        }
    }

    /// Transform a lockfile into a mapping that is easier to query:
    /// Lockfile -> { (install_id, system): (package_descriptor, locked_package) }
    fn make_seed_mapping(
        seed: &Lockfile,
    ) -> HashMap<(&str, &str), (&ManifestPackageDescriptor, &LockedPackage)> {
        seed.packages
            .iter()
            .filter_map(|locked| {
                let system = locked.system().as_str();
                let install_id = locked.install_id();
                let descriptor = seed.manifest.install.inner().get(locked.install_id())?;
                Some(((install_id, system), (descriptor, locked)))
            })
            .collect()
    }

    /// Creates package groups from a flat map of (catalog) install descriptors
    ///
    /// A group is created for each unique combination of (`descriptor.package_group` ｘ `descriptor.systems``).
    /// If descriptor.systems is [None], a group with `default_system` is created for each `package_group`.
    /// Each group contains a list of package descriptors that belong to that group.
    ///
    /// `seed_lockfile` is used to provide existing derivations for packages that are already locked,
    /// e.g. by a previous lockfile.
    /// These packages are used to constrain the resolution.
    /// If a package in `manifest` does not have a corresponding package in `seed_lockfile`,
    /// that package will be unconstrained, allowing a first install.
    ///
    /// As package groups only apply to catalog descriptors,
    /// this function **ignores other [ManifestPackageDescriptor] variants**.
    /// Those are expected to be locked separately.
    ///
    /// Greenkeeping: this function seem to return a [Result]
    /// only due to parsing [System] strings to [PackageSystem].
    /// If we restricted systems earlier with a common `System` type,
    /// fallible conversions like that would be unnecessary,
    /// or would be pushed higher up.
    fn collect_package_groups(
        manifest: &Manifest,
        seed_lockfile: Option<&Lockfile>,
    ) -> Result<impl Iterator<Item = PackageGroup>, ResolveError> {
        let seed_locked_packages = seed_lockfile.map_or_else(HashMap::new, Self::make_seed_mapping);

        // Using a btree map to ensure consistent ordering
        let mut map = BTreeMap::new();

        let manifest_systems = manifest.options.systems.as_deref();

        let maybe_licenses = manifest
            .options
            .allow
            .licenses
            .clone()
            .and_then(|licenses| {
                if licenses.is_empty() {
                    None
                } else {
                    Some(licenses)
                }
            });

        for (install_id, manifest_descriptor) in manifest.install.inner().iter() {
            // package groups are only relevant to catalog descriptors
            let Some(manifest_descriptor) = manifest_descriptor.as_catalog_descriptor_ref() else {
                continue;
            };

            let resolved_descriptor_base = PackageDescriptor {
                install_id: install_id.clone(),
                attr_path: manifest_descriptor.pkg_path.clone(),
                derivation: None,
                version: manifest_descriptor.version.clone(),
                allow_pre_releases: manifest.options.semver.allow_pre_releases,
                allow_broken: manifest.options.allow.broken,
                // TODO: add support for insecure
                allow_insecure: None,
                allow_unfree: manifest.options.allow.unfree,
                allow_missing_builds: None,
                allowed_licenses: maybe_licenses.clone(),
                systems: vec![],
            };

            let group_name = manifest_descriptor
                .pkg_group
                .as_deref()
                .unwrap_or(DEFAULT_GROUP_NAME);

            let resolved_group =
                map.entry(group_name.to_string())
                    .or_insert_with(|| PackageGroup {
                        descriptors: Vec::new(),
                        name: group_name.to_string(),
                    });

            let systems = {
                let available_systems = manifest_systems.unwrap_or(&*DEFAULT_SYSTEMS_STR);

                let package_systems = manifest_descriptor.systems.as_deref();

                for system in package_systems.into_iter().flatten() {
                    if !available_systems.contains(system) {
                        return Err(ResolveError::SystemUnavailableInManifest {
                            install_id: install_id.clone(),
                            system: system.to_string(),
                            enabled_systems: available_systems
                                .iter()
                                .map(|s| s.to_string())
                                .collect(),
                        });
                    }
                }

                package_systems
                    .or(manifest_systems)
                    .unwrap_or(&*DEFAULT_SYSTEMS_STR)
                    .iter()
                    .sorted()
                    .map(|s| {
                        PackageSystem::from_str(s)
                            .map_err(|_| ResolveError::UnrecognizedSystem(s.to_string()))
                    })
                    .collect::<Result<Vec<_>, _>>()?
            };

            for system in systems {
                // If the package was just added to the manifest, it will be missing in the seed,
                // which is derived from the _previous_ lockfile.
                // In this case, the derivation will be None, and the package will be unconstrained.
                //
                // If the package was already locked, but the descriptor has changed in a way
                // that invalidates the existing resolution, the derivation will be None.
                //
                // If the package was locked from a flake installable before
                // it needs to be re-resolved with the catalog, so the derivation will be None.
                let locked_derivation = seed_locked_packages
                    .get(&(install_id, &system.to_string()))
                    .filter(|(descriptor, _)| {
                        !descriptor.invalidates_existing_resolution(&manifest_descriptor.into())
                    })
                    .and_then(|(_, locked_package)| locked_package.as_catalog_package_ref())
                    .map(|locked_package| locked_package.derivation.clone());

                let mut resolved_descriptor = resolved_descriptor_base.clone();

                resolved_descriptor.systems = vec![system];
                resolved_descriptor.derivation = locked_derivation;

                resolved_group.descriptors.push(resolved_descriptor);
            }
        }
        Ok(map.into_values())
    }

    /// Eliminate groups that are already fully locked
    /// by extracting them into a separate list of locked packages.
    ///
    /// This is used to avoid re-resolving packages that are already locked.
    fn split_fully_locked_groups(
        groups: impl IntoIterator<Item = PackageGroup>,
        seed_lockfile: Option<&Lockfile>,
    ) -> (Vec<LockedPackage>, Vec<PackageGroup>) {
        let seed_locked_packages = seed_lockfile.map_or_else(HashMap::new, Self::make_seed_mapping);

        let (already_locked_groups, groups_to_lock): (Vec<_>, Vec<_>) =
            groups.into_iter().partition(|group| {
                group
                    .descriptors
                    .iter()
                    .all(|descriptor| descriptor.derivation.is_some())
            });

        // convert already locked groups back to locked packages
        let already_locked_packages = already_locked_groups
            .iter()
            .flat_map(|group| &group.descriptors)
            .flat_map(|descriptor| {
                std::iter::repeat(&descriptor.install_id).zip(&descriptor.systems)
            })
            .filter_map(|(install_id, system)| {
                seed_locked_packages
                    .get(&(install_id, &system.to_string()))
                    .map(|(_, locked_package)| (*locked_package).to_owned())
            })
            .collect::<Vec<_>>();

        (already_locked_packages, groups_to_lock)
    }

    /// Convert resolution results into a list of locked packages
    ///
    /// * Flattens `Group(Page(PackageResolutionInfo+)+)` into `LockedPackageCatalog+`
    /// * Adds a `system` field to each locked package.
    /// * Converts [serde_json::Value] based `outputs` and `outputs_to_install` fields
    ///   into [`IndexMap<String, String>`] and [`Vec<String>`] respectively.
    ///
    /// TODO: handle results from multiple pages
    ///       currently there is no api to request packages from specific pages
    /// TODO: handle json value conversion earlier in the shim (or the upstream spec)
    fn locked_packages_from_resolution<'manifest>(
        manifest: &'manifest Manifest,
        groups: impl IntoIterator<Item = ResolvedPackageGroup> + 'manifest,
    ) -> Result<impl Iterator<Item = LockedPackageCatalog> + 'manifest, ResolveError> {
        let groups = groups.into_iter().collect::<Vec<_>>();
        let failed_group_indices = Self::detect_failed_resolutions(&groups);
        let failures = if failed_group_indices.is_empty() {
            tracing::debug!("no resolution failures detected");
            None
        } else {
            tracing::debug!("resolution failures detected");
            let failed_groups = failed_group_indices
                .iter()
                .map(|&i| groups[i].clone())
                .collect::<Vec<_>>();
            let failures = Self::collect_failures(&failed_groups, manifest)?;
            Some(failures)
        };
        if let Some(failures) = failures {
            if !failures.is_empty() {
                tracing::debug!(n = failures.len(), "returning resolution failures");
                return Err(ResolveError::ResolutionFailed(ResolutionFailures(failures)));
            }
        }
        let locked_pkg_iter = groups
            .into_iter()
            .flat_map(|group| {
                group
                    .page
                    .and_then(|p| p.packages.clone())
                    .map(|pkgs| pkgs.into_iter())
                    .ok_or(ResolveError::ResolutionFailed(
                        // This should be unreachable, otherwise we would have detected
                        // it as a failure
                        ResolutionFailure::FallbackMessage {
                            msg: "catalog page wasn't complete".into(),
                        }
                        .into(),
                    ))
            })
            .flatten()
            .filter_map(|resolved_pkg| {
                manifest
                    .catalog_pkg_descriptor_with_id(&resolved_pkg.install_id)
                    .map(|descriptor| LockedPackageCatalog::from_parts(resolved_pkg, descriptor))
            });
        Ok(locked_pkg_iter)
    }

    /// Constructs [ResolutionFailure]s from the failed groups
    fn collect_failures(
        failed_groups: &[ResolvedPackageGroup],
        manifest: &Manifest,
    ) -> Result<Vec<ResolutionFailure>, ResolveError> {
        let mut failures = Vec::new();
        for group in failed_groups {
            tracing::debug!(
                name = group.name,
                "collecting failures from unresolved group"
            );
            for res_msg in group.msgs.iter() {
                tracing::debug!(
                    level = res_msg.level().to_string(),
                    msg = res_msg.msg(),
                    "handling resolution message"
                );
                // If it's not an error, skip this message
                if res_msg.level() != MessageLevel::Error {
                    continue;
                }
                let failure = match res_msg {
                    catalog::ResolutionMessage::General(inner) => {
                        tracing::debug!(kind = "general");
                        ResolutionFailure::FallbackMessage {
                            msg: inner.msg.clone(),
                        }
                    },
                    catalog::ResolutionMessage::AttrPathNotFoundNotInCatalog(inner) => {
                        tracing::debug!(kind = "attr_path_not_found.not_in_catalog",);
                        ResolutionFailure::PackageNotFound(inner.clone())
                    },
                    catalog::ResolutionMessage::AttrPathNotFoundNotFoundForAllSystems(inner) => {
                        tracing::debug!(kind = "attr_path_not_found.not_found_for_all_systems",);
                        ResolutionFailure::PackageUnavailableOnSomeSystems {
                            catalog_message: inner.clone(),
                            invalid_systems: Self::determine_invalid_systems(inner, manifest)?,
                        }
                    },
                    catalog::ResolutionMessage::AttrPathNotFoundSystemsNotOnSamePage(inner) => {
                        tracing::debug!(kind = "attr_path_not_found.systems_not_on_same_page");
                        ResolutionFailure::SystemsNotOnSamePage(inner.clone())
                    },
                    catalog::ResolutionMessage::ConstraintsTooTight(inner) => {
                        tracing::debug!(kind = "constraints_too_tight",);
                        ResolutionFailure::ConstraintsTooTight {
                            catalog_message: inner.clone(),
                            group: group.name.clone(),
                        }
                    },
                    catalog::ResolutionMessage::Unknown(inner) => {
                        tracing::debug!(
                            kind = "unknown",
                            msg_type = inner.msg_type,
                            context = serde_json::to_string(&inner.context).unwrap(),
                            "handling unknown resolution message"
                        );
                        ResolutionFailure::UnknownServiceMessage(inner.clone())
                    },
                };
                failures.push(failure);
            }
        }
        Ok(failures)
    }

    /// Determines which systems a package was requested on that it is not
    /// available for
    fn determine_invalid_systems(
        r_msg: &MsgAttrPathNotFoundNotFoundForAllSystems,
        manifest: &Manifest,
    ) -> Result<Vec<System>, ResolveError> {
        let default_systems = HashSet::<_>::from_iter(DEFAULT_SYSTEMS_STR.iter());
        let valid_systems = HashSet::<_>::from_iter(&r_msg.valid_systems);
        let manifest_systems = manifest
            .options
            .systems
            .as_ref()
            .map(HashSet::<_>::from_iter)
            .unwrap_or(default_systems);
        let pkg_descriptor = manifest
            .catalog_pkg_descriptor_with_id(&r_msg.install_id)
            .ok_or(ResolveError::InstallIdNotInManifest(
                r_msg.install_id.clone(),
            ))?;
        let pkg_systems = pkg_descriptor.systems.as_ref().map(HashSet::from_iter);
        let requested_systems = pkg_systems.unwrap_or(manifest_systems);
        let difference = &requested_systems - &valid_systems;
        Ok(Vec::from_iter(difference.into_iter().cloned()))
    }

    /// Detects whether any groups failed to resolve
    fn detect_failed_resolutions(groups: &[ResolvedPackageGroup]) -> Vec<usize> {
        groups
            .iter()
            .enumerate()
            .filter_map(|(idx, group)| {
                if group.page.is_none() {
                    tracing::debug!(name = group.name, "detected unresolved group");
                    Some(idx)
                } else if group.page.as_ref().is_some_and(|p| !p.complete) {
                    tracing::debug!(name = group.name, "detected incomplete page");
                    Some(idx)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    }

    /// Collect flake installable descriptors from the manifest and create a list of
    /// [FlakeInstallableToLock] to be resolved.
    /// Each descriptor is resolved once per system supported by the manifest,
    /// or other if not specified, for each system in [DEFAULT_SYSTEMS_STR].
    ///
    /// Unlike catalog packages, [FlakeInstallableToLock] are not affected by a seed lockfile.
    /// Already locked flake installables are split from the list in the second step using
    /// [Self::split_locked_flake_installables], based on the descriptor alone,
    /// no additional "marking" is needed.
    fn collect_flake_installables(
        manifest: &Manifest,
    ) -> impl Iterator<Item = FlakeInstallableToLock> + '_ {
        manifest
            .install
            .inner()
            .iter()
            .filter_map(|(install_id, descriptor)| {
                descriptor
                    .as_flake_descriptor_ref()
                    .map(|d| (install_id, d))
            })
            .flat_map(|(iid, d)| {
                let systems = if let Some(ref d_systems) = d.systems {
                    d_systems.as_slice()
                } else {
                    manifest
                        .options
                        .systems
                        .as_deref()
                        .unwrap_or(&*DEFAULT_SYSTEMS_STR)
                };
                systems.iter().map(move |s| FlakeInstallableToLock {
                    install_id: iid.clone(),
                    descriptor: d.clone(),
                    system: s.clone(),
                })
            })
    }

    /// Split a list of flake installables into already Locked packages ([LockedPackage])
    /// and yet to lock [FlakeInstallableToLock].
    ///
    /// This is equivalent to [Self::split_fully_locked_groups] but for flake installables.
    /// where `installables` are the flake installables found in a lockfile,
    /// with [Self::collect_flake_installables].
    fn split_locked_flake_installables(
        installables: impl IntoIterator<Item = FlakeInstallableToLock>,
        seed_lockfile: Option<&Lockfile>,
    ) -> (Vec<LockedPackage>, Vec<FlakeInstallableToLock>) {
        // todo: consider computing once and passing a reference to the consumer functions.
        //       we now compute this 3 times during a single lock operation
        let seed_locked_packages = seed_lockfile.map_or_else(HashMap::new, Self::make_seed_mapping);

        let by_id = installables.into_iter().group_by(|i| i.install_id.clone());

        let (already_locked, to_lock): (Vec<Vec<LockedPackage>>, Vec<Vec<FlakeInstallableToLock>>) =
            by_id.into_iter().partition_map(|(_, group)| {
                let unlocked = group.collect::<Vec<_>>();
                let mut locked = Vec::new();

                for installable in unlocked.iter() {
                    let Some((locked_descriptor, in_lockfile @ LockedPackage::Flake(_))) =
                        seed_locked_packages
                            .get(&(installable.install_id.as_str(), &installable.system))
                    else {
                        return Either::Right(unlocked);
                    };

                    if ManifestPackageDescriptor::from(installable.descriptor.clone())
                        .invalidates_existing_resolution(locked_descriptor)
                    {
                        return Either::Right(unlocked);
                    }

                    locked.push((*in_lockfile).to_owned());
                }
                Either::Left(locked)
            });

        let already_locked = already_locked.into_iter().flatten().collect();
        let to_lock = to_lock.into_iter().flatten().collect();

        (already_locked, to_lock)
    }

    /// Lock a set of flake installables and return the locked packages.
    /// Errors are collected into [ResolutionFailures] and returned as a single error.
    ///
    /// This is the eequivalent to
    /// [catalog::ClientTrait::resolve] and passing the result to [Self::locked_packages_from_resolution]
    /// in the context of flake installables.
    /// At this point flake installables are resolved sequentially.
    /// In further iterations we may want to resolve them in parallel,
    /// either here, through a method of [InstallableLocker],
    /// or the underlying `lock-flake-installable` primop itself.
    ///
    /// Todo: [ResolutionFailures] may be caught downstream and used to provide suggestions.
    ///       Those suggestions are invalid for the flake installables case.
    fn lock_flake_installables<'locking>(
        locking: &'locking impl InstallableLocker,
        installables: impl IntoIterator<Item = FlakeInstallableToLock> + 'locking,
    ) -> Result<impl Iterator<Item = LockedPackageFlake> + 'locking, ResolveError> {
        let mut ok = Vec::new();
        for installable in installables.into_iter() {
            match locking
                .lock_flake_installable(&installable.system, &installable.descriptor)
                .map(|locked_installable| {
                    LockedPackageFlake::from_parts(installable.install_id, locked_installable)
                }) {
                Ok(locked) => ok.push(locked),
                Err(e) => {
                    if let FlakeInstallableError::NixError(_) = e {
                        return Err(ResolveError::LockFlakeNixError(e));
                    }
                    let failure = ResolutionFailure::FallbackMessage { msg: e.to_string() };
                    return Err(ResolveError::ResolutionFailed(ResolutionFailures(vec![
                        failure,
                    ])));
                },
            }
        }
        Ok(ok.into_iter())
    }

    /// Collect store paths from the manifest and create a list of [LockedPackageStorePath].
    /// Since store paths are locked by definition,
    /// collection can directly map the discriptor to a locked package.
    fn collect_store_paths(manifest: &Manifest) -> Vec<LockedPackageStorePath> {
        manifest
            .install
            .inner()
            .iter()
            .filter_map(|(install_id, descriptor)| {
                descriptor
                    .as_store_path_descriptor_ref()
                    .map(|d| (install_id, d))
            })
            .flat_map(|(install_id, descriptor)| {
                let systems = if let Some(ref d_systems) = descriptor.systems {
                    d_systems.as_slice()
                } else {
                    manifest
                        .options
                        .systems
                        .as_deref()
                        .unwrap_or(&*DEFAULT_SYSTEMS_STR)
                };

                systems.iter().map(move |system| LockedPackageStorePath {
                    install_id: install_id.clone(),
                    store_path: descriptor.store_path.clone(),
                    system: system.clone(),
                    priority: descriptor.priority.unwrap_or(DEFAULT_PRIORITY),
                })
            })
            .collect()
    }

    /// Filter out packages from the locked manifest by install_id or group
    /// If groups_or_iids is empty, all packages are unlocked.
    ///
    /// This is used to create a seed lockfile to upgrade a subset of packages,
    /// as packages that are not in the seed lockfile will be re-resolved unconstrained.
    pub(crate) fn unlock_packages_by_group_or_iid(&mut self, groups_or_iids: &[&str]) -> &mut Self {
        if groups_or_iids.is_empty() {
            self.packages = Vec::new();
        } else {
            self.packages = std::mem::take(&mut self.packages)
                .into_iter()
                .filter(|package| {
                    if groups_or_iids.contains(&package.install_id()) {
                        return false;
                    }

                    if let Some(catalog_package) = package.as_catalog_package_ref() {
                        return !groups_or_iids.contains(&catalog_package.group.as_str());
                    }

                    true
                })
                .collect();
        }
        self
    }

    /// The manifest the user edits (i.e. not merged)
    pub fn user_manifest(&self) -> &Manifest {
        match &self.compose {
            Some(compose) => &compose.composer,
            None => &self.manifest,
        }
    }
}

/// Distinct types of packages that can be listed
/// TODO: drop in favor of mapping to `(ManifestPackageDescriptor*, LockedPackage*)`
#[derive(Debug, Clone, PartialEq)]
pub enum PackageToList {
    Catalog(PackageDescriptorCatalog, LockedPackageCatalog),
    Flake(PackageDescriptorFlake, LockedPackageFlake),
    StorePath(LockedPackageStorePath),
}

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("failed to resolve packages")]
    CatalogResolve(#[from] catalog::ResolveError),

    // todo: this should probably part of some validation logic of the manifest file
    //       rather than occurring during the locking process creation
    #[error("unrecognized system type: {0}")]
    UnrecognizedSystem(String),

    #[error("resolution failed: {0}")]
    ResolutionFailed(ResolutionFailures),

    // todo: this should probably part of some validation logic of the manifest file
    //       rather than occurring during the locking process creation
    #[error(
        "'{install_id}' specifies disabled or unknown system '{system}' (enabled systems: {enabled_systems})",
        enabled_systems=enabled_systems.join(", ")
    )]
    SystemUnavailableInManifest {
        install_id: String,
        system: String,
        enabled_systems: Vec<String>,
    },

    #[error(
        "The package '{0}' has license '{1}' which is not in the list of allowed licenses.\n\nAllow this license by adding it to 'options.allow.licenses' in manifest.toml"
    )]
    LicenseNotAllowed(String, String),
    #[error(
        "The package '{0}' is marked as broken.\n\nAllow broken packages by setting 'options.allow.broken = true' in manifest.toml"
    )]
    BrokenNotAllowed(String),
    #[error(
        "The package '{0}' has an unfree license.\n\nAllow unfree packages by setting 'options.allow.unfree = true' in manifest.toml"
    )]
    UnfreeNotAllowed(String),

    #[error("Corrupt manifest; couldn't find flake package descriptor for locked install_id '{0}'")]
    MissingPackageDescriptor(String),

    #[error(transparent)]
    LockFlakeNixError(FlakeInstallableError),
    #[error("catalog returned install id not in manifest: {0}")]
    InstallIdNotInManifest(String),
}

/// Errors that occur during merging a manifest that flox edit can recover from
#[derive(Debug, Error)]
pub enum RecoverableMergeError {
    #[error(transparent)]
    Merge(MergeError),

    #[error("failed to fetch environment '{include}'")]
    Fetch {
        include: IncludeDescriptor,
        #[source]
        err: Box<EnvironmentError>,
    },

    #[error(
        "cannot include environment since its manifest and lockfile are out of sync\n\
         \n\
         To resolve this issue run 'flox edit -d {0}' and retry\n"
    )]
    PathOutOfSync(PathBuf),

    #[error(
        "cannot include environment since it has changes not yet synced to a generation.\n\
         \n\
         To resolve this issue, run either\n\
         * 'flox edit -d {0} --sync' to commit your local changes to a new generation\n\
         * 'flox edit -d {0} --reset' to discard your local changes and reset to the latest generation\n"
    )]
    ManagedOutOfSync(PathBuf),

    /// Use this error when we don't need an error type to reuse in multiple places
    #[error("{0}")]
    Catchall(String),

    #[error("remote environments cannot include local environments")]
    RemoteCannotIncludeLocal,
}

pub mod test_helpers {
    use super::*;
    use crate::models::manifest::typed::PackageDescriptorStorePath;

    pub fn fake_catalog_package_lock(
        name: &str,
        group: Option<&str>,
    ) -> (String, ManifestPackageDescriptor, LockedPackageCatalog) {
        let install_id = format!("{}_install_id", name);

        let descriptor = PackageDescriptorCatalog {
            pkg_path: name.to_string(),
            pkg_group: group.map(|s| s.to_string()),
            systems: Some(vec![PackageSystem::Aarch64Darwin.to_string()]),
            version: None,
            priority: None,
        }
        .into();

        let locked = LockedPackageCatalog {
            attr_path: name.to_string(),
            broken: None,
            derivation: "derivation".to_string(),
            description: None,
            install_id: install_id.clone(),
            license: None,
            locked_url: "".to_string(),
            name: name.to_string(),
            outputs: Default::default(),
            outputs_to_install: None,
            pname: name.to_string(),
            rev: "".to_string(),
            rev_count: 0,
            rev_date: chrono::DateTime::parse_from_rfc3339("2021-08-31T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::offset::Utc),
            scrape_date: chrono::DateTime::parse_from_rfc3339("2021-08-31T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::offset::Utc),
            stabilities: None,
            unfree: None,
            version: "".to_string(),
            system: PackageSystem::Aarch64Darwin.to_string(),
            group: group.unwrap_or(DEFAULT_GROUP_NAME).to_string(),
            priority: 5,
        };
        (install_id, descriptor, locked)
    }

    pub fn fake_flake_installable_lock(
        name: &str,
    ) -> (String, PackageDescriptorFlake, LockedPackageFlake) {
        let install_id = format!("{}_install_id", name);

        let descriptor = PackageDescriptorFlake {
            flake: format!("github:nowhere/exciting#{name}"),
            priority: None,
            systems: None,
        };

        let locked = LockedPackageFlake {
            install_id: install_id.clone(),
            locked_installable: LockedInstallable {
                locked_url: format!(
                    "github:nowhere/exciting/affeaffeaffeaffeaffeaffeaffeaffeaffeaffe#{name}"
                ),
                flake_description: None,
                locked_flake_attr_path: format!("packages.aarch64-darwin.{name}"),
                derivation: "derivation".to_string(),
                outputs: Default::default(),
                output_names: vec![],
                outputs_to_install: None,
                requested_outputs_to_install: None,
                package_system: "aarch64-darwin".to_string(),
                system: "aarch64-darwin".to_string(),
                name: format!("{name}-1.0.0"),
                pname: Some(name.to_string()),
                version: Some("1.0.0".to_string()),
                description: None,
                licenses: None,
                broken: None,
                unfree: None,
                priority: DEFAULT_PRIORITY,
            },
        };
        (install_id, descriptor, locked)
    }

    pub fn fake_store_path_lock(
        name: &str,
    ) -> (String, PackageDescriptorStorePath, LockedPackageStorePath) {
        let install_id = format!("{}_install_id", name);

        let descriptor = PackageDescriptorStorePath {
            store_path: format!("/nix/store/{}", name),
            systems: Some(vec![PackageSystem::Aarch64Darwin.to_string()]),
            priority: None,
        };

        let locked = LockedPackageStorePath {
            install_id: install_id.clone(),
            store_path: format!("/nix/store/{}", name),
            system: PackageSystem::Aarch64Darwin.to_string(),
            priority: DEFAULT_PRIORITY,
        };
        (install_id, descriptor, locked)
    }

    pub fn nix_eval_jobs_descriptor() -> PackageDescriptorFlake {
        PackageDescriptorFlake {
            flake: "github:nix-community/nix-eval-jobs".to_string(),
            priority: None,
            systems: None,
        }
    }

    /// This JSON was copied from a manifest.lock after installing github:nix-community/nix-eval-jobs
    pub static LOCKED_NIX_EVAL_JOBS: LazyLock<LockedPackageFlake> = LazyLock::new(|| {
        serde_json::from_str(r#"
            {
              "install_id": "nix-eval-jobs",
              "locked-url": "github:nix-community/nix-eval-jobs/c132534bc68eb48479a59a3116ee7ce0f16ce12b",
              "flake-description": "Hydra's builtin hydra-eval-jobs as a standalone",
              "locked-flake-attr-path": "packages.aarch64-darwin.default",
              "derivation": "/nix/store/29y1mrdqncdjfkdfa777zmspp9djzb6b-nix-eval-jobs-2.23.0.drv",
              "outputs": {
                "out": "/nix/store/qigv8kbk1gpk0g2pfw10lbmdy44cf06r-nix-eval-jobs-2.23.0"
              },
              "output-names": [
                "out"
              ],
              "outputs-to-install": [
                "out"
              ],
              "package-system": "aarch64-darwin",
              "system": "aarch64-darwin",
              "name": "nix-eval-jobs-2.23.0",
              "pname": "nix-eval-jobs",
              "version": "2.23.0",
              "description": "Hydra's builtin hydra-eval-jobs as a standalone",
              "licenses": [
                "GPL-3.0"
              ],
              "broken": false,
              "unfree": false
            }
        "#).unwrap()
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::LazyLock;
    use std::vec;

    use catalog::MsgUnknown;
    use catalog_api_v1::types::{PackageOutput, PackageOutputs};
    use indoc::indoc;
    use pollster::FutureExt;
    use pretty_assertions::assert_eq;
    use proptest::prelude::*;
    use test_helpers::{
        fake_catalog_package_lock,
        fake_flake_installable_lock,
        fake_store_path_lock,
    };

    use self::catalog::PackageResolutionInfo;
    use super::*;
    use crate::flox::EnvironmentRef;
    use crate::flox::test_helpers::{flox_instance, flox_instance_with_optional_floxhub};
    use crate::models::environment::Environment;
    use crate::models::environment::fetcher::test_helpers::mock_include_fetcher;
    use crate::models::environment::path_environment::test_helpers::{
        new_named_path_environment_in,
        new_path_environment,
        new_path_environment_in,
    };
    use crate::models::environment::path_environment::tests::generate_path_environments_without_install_or_include;
    use crate::models::environment::remote_environment::test_helpers::mock_remote_environment;
    use crate::models::manifest::raw::RawManifest;
    use crate::models::manifest::typed::{Include, Manifest, Vars};
    use crate::providers::catalog::test_helpers::{
        auto_recording_catalog_client,
        catalog_replay_client,
    };
    use crate::providers::catalog::{GENERATED_DATA, MockClient};
    use crate::providers::flake_installable_locker::{
        FlakeInstallableError,
        InstallableLockerMock,
    };

    struct PanickingLocker;
    impl InstallableLocker for PanickingLocker {
        fn lock_flake_installable(
            &self,
            _: impl AsRef<str>,
            _: &PackageDescriptorFlake,
        ) -> Result<LockedInstallable, FlakeInstallableError> {
            panic!("this flake locker always panics")
        }
    }

    static TEST_RAW_MANIFEST: LazyLock<RawManifest> = LazyLock::new(|| {
        indoc! {r#"
          version = 1

          [install]
          hello_install_id.pkg-path = "hello"
          hello_install_id.pkg-group = "group"

          [options]
          systems = ["aarch64-darwin"]
        "#}
        .parse()
        .unwrap()
    });

    static TEST_TYPED_MANIFEST: LazyLock<Manifest> =
        LazyLock::new(|| TEST_RAW_MANIFEST.to_typed().unwrap());

    static TEST_RESOLUTION_PARAMS: LazyLock<Vec<PackageGroup>> = LazyLock::new(|| {
        vec![PackageGroup {
            name: "group".to_string(),
            descriptors: vec![PackageDescriptor {
                install_id: "hello_install_id".to_string(),
                attr_path: "hello".to_string(),
                derivation: None,
                version: None,
                allow_pre_releases: None,
                allow_broken: None,
                allow_insecure: None,
                allow_unfree: None,
                allowed_licenses: None,
                allow_missing_builds: None,
                systems: vec![PackageSystem::Aarch64Darwin],
            }],
        }]
    });

    static TEST_LOCKED_MANIFEST: LazyLock<Lockfile> = LazyLock::new(|| Lockfile {
        version: Version::<1>,
        manifest: TEST_TYPED_MANIFEST.clone(),
        packages: vec![
            LockedPackageCatalog {
                attr_path: "hello".to_string(),
                broken: Some(false),
                derivation: "derivation".to_string(),
                description: Some("description".to_string()),
                install_id: "hello_install_id".to_string(),
                license: Some("license".to_string()),
                locked_url: "locked_url".to_string(),
                name: "hello".to_string(),
                outputs: [("name".to_string(), "store_path".to_string())]
                    .into_iter()
                    .collect(),

                outputs_to_install: Some(vec!["name".to_string()]),
                pname: "pname".to_string(),
                rev: "rev".to_string(),
                rev_count: 1,
                rev_date: chrono::DateTime::parse_from_rfc3339("2021-08-31T00:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::offset::Utc),
                scrape_date: chrono::DateTime::parse_from_rfc3339("2021-08-31T00:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::offset::Utc),
                stabilities: Some(vec!["stability".to_string()]),
                unfree: Some(false),
                version: "version".to_string(),
                system: PackageSystem::Aarch64Darwin.to_string(),
                group: "group".to_string(),
                priority: 5,
            }
            .into(),
        ],
        compose: None,
    });

    #[test]
    fn make_params_smoke() {
        let manifest = &*TEST_TYPED_MANIFEST;

        let params = Lockfile::collect_package_groups(manifest, None)
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(&params, &*TEST_RESOLUTION_PARAMS);
    }

    /// When `options.systems` defines multiple systems,
    /// request groups for each system separately.
    #[test]
    fn make_params_multiple_systems() {
        let manifest_str = indoc! {r#"
            version = 1

            [install]
            vim.pkg-path = "vim"
            emacs.pkg-path = "emacs"

            [options]
            systems = ["aarch64-darwin", "x86_64-linux"]
        "#};
        let manifest = toml::from_str(manifest_str).unwrap();

        let expected_params = vec![PackageGroup {
            name: DEFAULT_GROUP_NAME.to_string(),
            descriptors: vec![
                PackageDescriptor {
                    allow_pre_releases: None,
                    attr_path: "emacs".to_string(),
                    derivation: None,
                    install_id: "emacs".to_string(),
                    version: None,
                    allow_broken: None,
                    allow_insecure: None,
                    allow_unfree: None,
                    allowed_licenses: None,
                    allow_missing_builds: None,
                    systems: vec![PackageSystem::Aarch64Darwin],
                },
                PackageDescriptor {
                    allow_pre_releases: None,
                    attr_path: "emacs".to_string(),
                    derivation: None,
                    install_id: "emacs".to_string(),
                    version: None,
                    allow_broken: None,
                    allow_insecure: None,
                    allow_unfree: None,
                    allowed_licenses: None,
                    allow_missing_builds: None,
                    systems: vec![PackageSystem::X8664Linux],
                },
                PackageDescriptor {
                    allow_pre_releases: None,
                    attr_path: "vim".to_string(),
                    derivation: None,
                    install_id: "vim".to_string(),
                    version: None,
                    allow_broken: None,
                    allow_insecure: None,
                    allow_unfree: None,
                    allowed_licenses: None,
                    allow_missing_builds: None,
                    systems: vec![PackageSystem::Aarch64Darwin],
                },
                PackageDescriptor {
                    allow_pre_releases: None,
                    attr_path: "vim".to_string(),
                    derivation: None,
                    install_id: "vim".to_string(),
                    version: None,
                    allow_broken: None,
                    allow_insecure: None,
                    allow_unfree: None,
                    allowed_licenses: None,
                    allow_missing_builds: None,
                    systems: vec![PackageSystem::X8664Linux],
                },
            ],
        }];

        let actual_params = Lockfile::collect_package_groups(&manifest, None)
            .unwrap()
            .collect::<Vec<_>>();

        assert_eq!(actual_params, expected_params);
    }

    /// When `options.systems` defines multiple systems,
    /// request groups for each system separately.
    /// If a package specifies systems, use those instead.
    #[test]
    fn make_params_limit_systems() {
        let manifest_str = indoc! {r#"
            version = 1

            [install]
            vim.pkg-path = "vim"
            emacs.pkg-path = "emacs"
            emacs.systems = ["aarch64-darwin" ]

            [options]
            systems = ["aarch64-darwin", "x86_64-linux"]
        "#};
        let manifest = toml::from_str(manifest_str).unwrap();

        let expected_params = vec![PackageGroup {
            name: DEFAULT_GROUP_NAME.to_string(),
            descriptors: vec![
                PackageDescriptor {
                    allow_pre_releases: None,
                    attr_path: "emacs".to_string(),
                    install_id: "emacs".to_string(),
                    derivation: None,
                    version: None,
                    allow_broken: None,
                    allow_insecure: None,
                    allow_unfree: None,
                    allowed_licenses: None,
                    allow_missing_builds: None,
                    systems: vec![PackageSystem::Aarch64Darwin],
                },
                PackageDescriptor {
                    allow_pre_releases: None,
                    attr_path: "vim".to_string(),
                    derivation: None,
                    install_id: "vim".to_string(),
                    version: None,
                    allow_broken: None,
                    allow_insecure: None,
                    allow_unfree: None,
                    allowed_licenses: None,
                    allow_missing_builds: None,
                    systems: vec![PackageSystem::Aarch64Darwin],
                },
                PackageDescriptor {
                    allow_pre_releases: None,
                    attr_path: "vim".to_string(),
                    derivation: None,
                    install_id: "vim".to_string(),
                    version: None,
                    allow_broken: None,
                    allow_insecure: None,
                    allow_unfree: None,
                    allowed_licenses: None,
                    allow_missing_builds: None,
                    systems: vec![PackageSystem::X8664Linux],
                },
            ],
        }];

        let actual_params = Lockfile::collect_package_groups(&manifest, None)
            .unwrap()
            .collect::<Vec<_>>();

        assert_eq!(actual_params, expected_params);
    }

    /// If a package specifies a system not in `options.systems`,
    /// return an error.
    #[test]
    fn descriptor_system_required_in_options() {
        let manifest_str = indoc! {r#"
            version = 1

            [install]
            vim.pkg-path = "vim"
            emacs.pkg-path = "emacs"
            emacs.systems = ["aarch64-darwin" ]

            [options]
            systems = ["x86_64-linux"]
        "#};

        // todo: ideally the manifest would not even parse if it has an unavailable system
        let manifest = toml::from_str(manifest_str).unwrap();

        let actual_result = Lockfile::collect_package_groups(&manifest, None);

        assert!(
            matches!(actual_result, Err(ResolveError::SystemUnavailableInManifest {
                install_id,
                system,
                enabled_systems
            }) if install_id == "emacs" && system == "aarch64-darwin" && enabled_systems == vec!["x86_64-linux"])
        );
    }

    /// If packages specify different groups,
    /// create request groups for each group.
    #[test]
    fn make_params_groups() {
        let manifest_str = indoc! {r#"
            version = 1

            [install]
            vim.pkg-path = "vim"
            vim.pkg-group = "group1"

            emacs.pkg-path = "emacs"
            emacs.pkg-group = "group2"

            [options]
            systems = ["aarch64-darwin"]
        "#};

        let manifest = toml::from_str(manifest_str).unwrap();

        let expected_params = vec![
            PackageGroup {
                name: "group1".to_string(),
                descriptors: vec![PackageDescriptor {
                    allow_pre_releases: None,
                    attr_path: "vim".to_string(),
                    derivation: None,
                    install_id: "vim".to_string(),
                    version: None,
                    allow_broken: None,
                    allow_insecure: None,
                    allow_unfree: None,
                    allowed_licenses: None,
                    allow_missing_builds: None,
                    systems: vec![PackageSystem::Aarch64Darwin],
                }],
            },
            PackageGroup {
                name: "group2".to_string(),
                descriptors: vec![PackageDescriptor {
                    allow_pre_releases: None,
                    attr_path: "emacs".to_string(),
                    derivation: None,
                    install_id: "emacs".to_string(),
                    version: None,
                    allow_broken: None,
                    allow_insecure: None,
                    allow_unfree: None,
                    allowed_licenses: None,
                    allow_missing_builds: None,
                    systems: vec![PackageSystem::Aarch64Darwin],
                }],
            },
        ];

        let actual_params = Lockfile::collect_package_groups(&manifest, None)
            .unwrap()
            .collect::<Vec<_>>();

        assert_eq!(actual_params, expected_params);
    }

    /// If a seed mapping is provided, use the derivations from the seed where possible
    #[test]
    fn make_params_seeded() {
        let mut manifest = TEST_TYPED_MANIFEST.clone();

        // Add a package to the manifest that is not already locked
        manifest.install.inner_mut().insert(
            "unlocked".to_string(),
            PackageDescriptorCatalog {
                pkg_path: "unlocked".to_string(),
                pkg_group: Some("group".to_string()),
                systems: None,
                version: None,
                priority: None,
            }
            .into(),
        );

        let actual_params =
            Lockfile::collect_package_groups(&manifest, Some(&*TEST_LOCKED_MANIFEST))
                .unwrap()
                .collect::<Vec<_>>();

        let expected_params = vec![PackageGroup {
            name: "group".to_string(),
            descriptors: vec![
                // 'hello' was already locked, so it should have a derivation
                PackageDescriptor {
                    allow_pre_releases: None,
                    attr_path: "hello".to_string(),
                    derivation: Some("derivation".to_string()),
                    install_id: "hello_install_id".to_string(),
                    version: None,
                    allow_broken: None,
                    allow_insecure: None,
                    allow_unfree: None,
                    allowed_licenses: None,
                    allow_missing_builds: None,
                    systems: vec![PackageSystem::Aarch64Darwin],
                },
                // The unlocked package should not have a derivation
                PackageDescriptor {
                    allow_pre_releases: None,
                    attr_path: "unlocked".to_string(),
                    derivation: None,
                    install_id: "unlocked".to_string(),
                    version: None,
                    allow_broken: None,
                    allow_insecure: None,
                    allow_unfree: None,
                    allowed_licenses: None,
                    allow_missing_builds: None,
                    systems: vec![PackageSystem::Aarch64Darwin],
                },
            ],
        }];

        assert_eq!(actual_params, expected_params);
    }

    /// If a seed mapping is provided, use the derivations from the seed where possible
    /// 1) If the package is unchanged, it should not be re-resolved.
    #[test]
    fn make_params_seeded_unchanged() {
        let (foo_before_iid, foo_before_descriptor, foo_before_locked) =
            fake_catalog_package_lock("foo", None);
        let mut manifest_before = Manifest::default();
        manifest_before
            .install
            .inner_mut()
            .insert(foo_before_iid.clone(), foo_before_descriptor.clone());

        let seed = Lockfile {
            version: Version::<1>,
            manifest: manifest_before.clone(),
            packages: vec![foo_before_locked.clone().into()],
            compose: None,
        };

        // ---------------------------------------------------------------------

        let actual_params = Lockfile::collect_package_groups(&manifest_before, Some(&seed))
            .unwrap()
            .collect::<Vec<_>>();

        // the original derivation should be present and unchanged
        assert_eq!(
            actual_params[0].descriptors[0].derivation.as_ref(),
            Some(&foo_before_locked.derivation)
        );
    }

    /// If a seed mapping is provided, use the derivations from the seed where possible
    /// 2) Changes that invalidate the locked package should cause it to be re-resolved.
    ///    Here, the package path is changed.
    #[test]
    fn make_params_seeded_unlock_if_invalidated() {
        let (foo_before_iid, foo_before_descriptor, foo_before_locked) =
            fake_catalog_package_lock("foo", None);
        let mut manifest_before = Manifest::default();
        manifest_before
            .install
            .inner_mut()
            .insert(foo_before_iid.clone(), foo_before_descriptor.clone());

        let seed = Lockfile {
            version: Version::<1>,
            manifest: manifest_before.clone(),
            packages: vec![foo_before_locked.into()],
            compose: None,
        };

        // ---------------------------------------------------------------------

        let (foo_after_iid, mut foo_after_descriptor, _) = fake_catalog_package_lock("foo", None);

        if let ManifestPackageDescriptor::Catalog(ref mut descriptor) = foo_after_descriptor {
            descriptor.pkg_path = "bar".to_string();
        } else {
            panic!("Expected a catalog descriptor");
        };

        assert!(foo_after_descriptor.invalidates_existing_resolution(&foo_before_descriptor));

        let mut manifest_after = Manifest::default();
        manifest_after
            .install
            .inner_mut()
            .insert(foo_after_iid.clone(), foo_after_descriptor.clone());

        let actual_params = Lockfile::collect_package_groups(&manifest_after, Some(&seed))
            .unwrap()
            .collect::<Vec<_>>();

        // if the package changed, it should be re-resolved
        // i.e. the derivation should be None
        assert_eq!(actual_params[0].descriptors[0].derivation.as_ref(), None);
    }

    /// If a seed mapping is provided, use the derivations from the seed where possible
    /// 3) Changes to the descriptor that do not invalidate the derivation
    ///    should not cause it to be re-resolved.
    ///    Here, the priority is changed.
    #[test]
    fn make_params_seeded_changed_no_invalidation() {
        let (foo_before_iid, foo_before_descriptor, foo_before_locked) =
            fake_catalog_package_lock("foo", None);
        let mut manifest_before = Manifest::default();
        manifest_before
            .install
            .inner_mut()
            .insert(foo_before_iid.clone(), foo_before_descriptor.clone());

        let seed = Lockfile {
            version: Version::<1>,
            manifest: manifest_before.clone(),
            packages: vec![foo_before_locked.clone().into()],
            compose: None,
        };

        // ---------------------------------------------------------------------

        let (foo_after_iid, mut foo_after_descriptor, _) = fake_catalog_package_lock("foo", None);
        if let ManifestPackageDescriptor::Catalog(ref mut descriptor) = foo_after_descriptor {
            descriptor.priority = Some(10);
        } else {
            panic!("Expected a catalog descriptor");
        };

        assert!(!foo_after_descriptor.invalidates_existing_resolution(&foo_before_descriptor));

        let mut manifest_after = Manifest::default();
        manifest_after
            .install
            .inner_mut()
            .insert(foo_after_iid.clone(), foo_after_descriptor.clone());

        let actual_params = Lockfile::collect_package_groups(&manifest_after, Some(&seed))
            .unwrap()
            .collect::<Vec<_>>();

        assert_eq!(
            actual_params[0].descriptors[0].derivation.as_ref(),
            Some(&foo_before_locked.derivation)
        );
    }

    /// If flake installables and catalog packages are mixed,
    /// [Lockfile::collect_package_groups]
    /// should only return [PackageGroup]s for the catalog descriptors.
    #[test]
    fn make_params_filters_installables() {
        let manifest_str = indoc! {r#"
            version = 1

            [install]
            vim.pkg-path = "vim"
            emacs.flake = "github:nixos/nixpkgs#emacs"

            [options]
            systems = ["aarch64-darwin", "x86_64-linux"]
        "#};
        let manifest = toml::from_str(manifest_str).unwrap();

        let expected_params = vec![PackageGroup {
            name: DEFAULT_GROUP_NAME.to_string(),
            descriptors: [PackageSystem::Aarch64Darwin, PackageSystem::X8664Linux]
                .map(|system| {
                    [PackageDescriptor {
                        allow_pre_releases: None,
                        attr_path: "vim".to_string(),
                        derivation: None,
                        install_id: "vim".to_string(),
                        version: None,
                        allow_broken: None,
                        allow_insecure: None,
                        allow_unfree: None,
                        allowed_licenses: None,
                        allow_missing_builds: None,
                        systems: vec![system],
                    }]
                })
                .into_iter()
                .flatten()
                .collect(),
        }];

        let actual_params = Lockfile::collect_package_groups(&manifest, None)
            .unwrap()
            .collect::<Vec<_>>();

        assert_eq!(actual_params, expected_params);
    }

    /// [Lockfile::collect_package_groups] generates [FlakeInstallableToLock]
    /// for each default system.
    #[test]
    fn make_installables_to_lock_for_default_systems() {
        let mut manifest = Manifest::default();
        let (foo_install_id, foo_descriptor, _) = fake_flake_installable_lock("foo");

        manifest
            .install
            .inner_mut()
            .insert(foo_install_id.clone(), foo_descriptor.clone().into());

        let expected = DEFAULT_SYSTEMS_STR
            .clone()
            .map(|system| FlakeInstallableToLock {
                install_id: foo_install_id.clone(),
                descriptor: foo_descriptor.clone(),
                system: system.to_string(),
            });

        let actual: Vec<_> = Lockfile::collect_flake_installables(&manifest).collect();

        assert_eq!(actual, expected);
    }

    /// [Lockfile::collect_package_groups] generates [FlakeInstallableToLock]
    /// for each system in the manifest.
    #[test]
    fn make_installables_to_lock_for_manifest_systems() {
        let system = "aarch64-darwin";

        let mut manifest = Manifest::default();
        manifest.options.systems = Some(vec![system.to_string()]);

        let (foo_install_id, foo_descriptor, _) = fake_flake_installable_lock("foo");

        manifest
            .install
            .inner_mut()
            .insert(foo_install_id.clone(), foo_descriptor.clone().into());

        let expected = [FlakeInstallableToLock {
            install_id: foo_install_id.clone(),
            descriptor: foo_descriptor.clone(),
            system: system.to_string(),
        }];

        let actual: Vec<_> = Lockfile::collect_flake_installables(&manifest).collect();

        assert_eq!(actual, expected);
    }

    /// If flake installables and catalog packages are mixed,
    /// [Lockfile::collect_flake_installables]
    /// should only return [FlakeInstallableToLock] for the flake installables.
    #[test]
    fn make_installables_to_lock_filter_catalog() {
        let mut manifest = Manifest::default();
        let (foo_install_id, foo_descriptor, _) = fake_flake_installable_lock("foo");
        let (bar_install_id, bar_descriptor, _) = fake_catalog_package_lock("bar", None);

        manifest
            .install
            .inner_mut()
            .insert(foo_install_id.clone(), foo_descriptor.clone().into());
        manifest
            .install
            .inner_mut()
            .insert(bar_install_id.clone(), bar_descriptor.clone());

        let expected = DEFAULT_SYSTEMS_STR
            .clone()
            .map(|system| FlakeInstallableToLock {
                install_id: foo_install_id.clone(),
                descriptor: foo_descriptor.clone(),
                system: system.to_string(),
            });

        let actual: Vec<_> = Lockfile::collect_flake_installables(&manifest).collect();

        assert_eq!(actual, expected);
    }

    #[test]
    fn ungroup_response() {
        let groups = vec![ResolvedPackageGroup {
            page: Some(CatalogPage {
                page: 1,
                complete: true,
                url: "url".to_string(),
                packages: Some(vec![PackageResolutionInfo {
                    catalog: None,
                    attr_path: "hello".to_string(),
                    pkg_path: "hello".to_string(),
                    broken: Some(false),
                    derivation: "derivation".to_string(),
                    description: Some("description".to_string()),
                    insecure: Some(false),
                    install_id: "hello_install_id".to_string(),
                    license: Some("license".to_string()),
                    locked_url: "locked_url".to_string(),
                    name: "hello".to_string(),
                    outputs: PackageOutputs(vec![PackageOutput {
                        name: "name".to_string(),
                        store_path: "store_path".to_string(),
                    }]),
                    outputs_to_install: Some(vec!["name".to_string()]),
                    pname: "pname".to_string(),
                    rev: "rev".to_string(),
                    rev_count: 1,
                    rev_date: chrono::DateTime::parse_from_rfc3339("2021-08-31T00:00:00Z")
                        .unwrap()
                        .with_timezone(&chrono::offset::Utc),
                    scrape_date: Some(
                        chrono::DateTime::parse_from_rfc3339("2021-08-31T00:00:00Z")
                            .unwrap()
                            .with_timezone(&chrono::offset::Utc),
                    ),
                    stabilities: Some(vec!["stability".to_string()]),
                    unfree: Some(false),
                    version: "version".to_string(),
                    system: PackageSystem::Aarch64Darwin,
                    cache_uri: None,
                    missing_builds: None,
                }]),
                msgs: vec![],
            }),
            name: "group".to_string(),
            msgs: vec![],
        }];

        let manifest = &*TEST_TYPED_MANIFEST;

        let locked_packages = Lockfile::locked_packages_from_resolution(manifest, groups.clone())
            .unwrap()
            .collect::<Vec<_>>();

        let descriptor = manifest
            .install
            .inner()
            .get(&groups[0].page.as_ref().unwrap().packages.as_ref().unwrap()[0].install_id)
            .and_then(ManifestPackageDescriptor::as_catalog_descriptor_ref)
            .expect("expected a catalog descriptor")
            .clone();

        assert_eq!(locked_packages.len(), 1);
        assert_eq!(
            locked_packages[0],
            LockedPackageCatalog::from_parts(
                groups[0].page.as_ref().unwrap().packages.as_ref().unwrap()[0].clone(),
                descriptor,
            )
        );
    }

    /// Unlocking by iid should remove only the package with that iid.
    /// Both catalog packages and flake installables should be removed.
    #[test]
    fn unlock_by_iid() {
        let mut manifest = Manifest::default();
        let (foo_iid, foo_descriptor, foo_locked) = fake_catalog_package_lock("foo", None);
        let (bar_iid, bar_descriptor, bar_locked) = fake_catalog_package_lock("bar", None);
        let (baz_iid, baz_descriptor, baz_locked) = fake_flake_installable_lock("baz");
        let (qux_iid, qux_descriptor, qux_locked) = fake_flake_installable_lock("qux");
        manifest
            .install
            .inner_mut()
            .insert(foo_iid.clone(), foo_descriptor);
        manifest
            .install
            .inner_mut()
            .insert(bar_iid.clone(), bar_descriptor);
        manifest
            .install
            .inner_mut()
            .insert(baz_iid.clone(), baz_descriptor.into());
        manifest
            .install
            .inner_mut()
            .insert(qux_iid.clone(), qux_descriptor.into());
        let mut lockfile = Lockfile {
            version: Version::<1>,
            manifest: manifest.clone(),
            packages: vec![
                foo_locked.into(),
                bar_locked.clone().into(),
                baz_locked.into(),
                qux_locked.clone().into(),
            ],
            compose: None,
        };

        lockfile.unlock_packages_by_group_or_iid(&[&foo_iid, &baz_iid]);

        assert_eq!(lockfile.packages, vec![
            bar_locked.into(),
            qux_locked.into()
        ]);
    }

    /// Unlocking by group should remove all packages in that group
    #[test]
    fn unlock_by_group() {
        let mut manifest = Manifest::default();
        let (foo_iid, foo_descriptor, foo_locked) = fake_catalog_package_lock("foo", Some("group"));
        let (bar_iid, bar_descriptor, bar_locked) = fake_catalog_package_lock("bar", Some("group"));
        manifest
            .install
            .inner_mut()
            .insert(foo_iid.clone(), foo_descriptor);
        manifest
            .install
            .inner_mut()
            .insert(bar_iid.clone(), bar_descriptor);
        let mut lockfile = Lockfile {
            version: Version::<1>,
            manifest: manifest.clone(),
            packages: vec![foo_locked.into(), bar_locked.into()],
            compose: None,
        };

        lockfile.unlock_packages_by_group_or_iid(&["group"]);

        assert_eq!(lockfile.packages, vec![]);
    }

    /// If an unlocked iid is also used as a group, remove both the group
    /// and the package
    #[test]
    fn unlock_by_iid_and_group() {
        let mut manifest = Manifest::default();
        let (foo_iid, foo_descriptor, foo_locked) =
            fake_catalog_package_lock("foo", Some("foo_install_id"));
        let (bar_iid, bar_descriptor, bar_locked) =
            fake_catalog_package_lock("bar", Some("foo_install_id"));
        manifest
            .install
            .inner_mut()
            .insert(foo_iid.clone(), foo_descriptor);
        manifest
            .install
            .inner_mut()
            .insert(bar_iid.clone(), bar_descriptor);
        let mut lockfile = Lockfile {
            version: Version::<1>,
            manifest: manifest.clone(),
            packages: vec![foo_locked.into(), bar_locked.into()],
            compose: None,
        };

        lockfile.unlock_packages_by_group_or_iid(&[&foo_iid]);

        assert_eq!(lockfile.packages, vec![]);
    }

    #[test]
    fn unlock_by_iid_noop_if_already_unlocked() {
        let mut seed = TEST_LOCKED_MANIFEST.clone();

        // If the package is not in the seed, the lockfile should be unchanged
        let expected = seed.packages.clone();

        seed.unlock_packages_by_group_or_iid(&["not in here"]);

        assert_eq!(seed.packages, expected,);
    }

    #[tokio::test]
    async fn locking_unknown_message() {
        let manifest = Manifest::from_str(indoc! {r#"
                version = 1

                [install]
                ps.pkg-path = "darwin.ps"

                [options]
                systems = [ "x86_64-darwin", "aarch64-linux" ]
            "#})
        .unwrap();

        let client = catalog_replay_client(
            GENERATED_DATA.join("resolve/darwin_ps_incompatible_transform_error_to_unknown.yaml"),
        )
        .await;

        let locked_manifest =
            Lockfile::resolve_manifest(&manifest, None, &client, &InstallableLockerMock::new())
                .await;
        if let Err(ResolveError::ResolutionFailed(res_failures)) = locked_manifest {
            assert_eq!(res_failures.to_string(), format!("\n{}", "unknown message"));
            if let [ResolutionFailure::UnknownServiceMessage(MsgUnknown { msg, .. })] =
                res_failures.0.as_slice()
            {
                assert_eq!(msg, "unknown message");
            } else {
                panic!(
                    "expected a single UnknownServiceMessage, got {:?}",
                    res_failures.0.as_slice()
                );
            }
        } else {
            panic!("expected resolution failure, got {:?}", locked_manifest);
        }
    }

    #[tokio::test]
    async fn locking_general_message() {
        let manifest = Manifest::from_str(indoc! {r#"
                version = 1

                [install]
                ps.pkg-path = "darwin.ps"

                [options]
                systems = [ "x86_64-darwin", "aarch64-linux" ]
            "#})
        .unwrap();

        let client = catalog_replay_client(
            GENERATED_DATA.join("resolve/darwin_ps_incompatible_transform_error_to_general.yaml"),
        )
        .await;

        let locked_manifest =
            Lockfile::resolve_manifest(&manifest, None, &client, &InstallableLockerMock::new())
                .await;
        if let Err(ResolveError::ResolutionFailed(res_failures)) = locked_manifest {
            assert_eq!(res_failures.to_string(), format!("\n{}", "general message"));
            if let [ResolutionFailure::FallbackMessage { msg, .. }] = res_failures.0.as_slice() {
                assert_eq!(msg, "general message");
            } else {
                panic!(
                    "expected a single FallbackMessage, got {:?}",
                    res_failures.0.as_slice()
                );
            }
        } else {
            panic!("expected resolution failure, got {:?}", locked_manifest);
        }
    }

    /// Test the server generated message is passed through for systems not on same page
    #[tokio::test]
    async fn locking_message_is_passed_through_for_systems_not_on_same_page() {
        let manifest = Manifest::from_str(indoc! {r#"
                version = 1

                [install]
                ps.pkg-path = "darwin.ps"

                [options]
                systems = [ "x86_64-darwin", "aarch64-linux" ]
            "#})
        .unwrap();
        let client =
            catalog_replay_client(GENERATED_DATA.join("resolve/darwin_ps_incompatible.yaml")).await;

        let locked_manifest =
            Lockfile::resolve_manifest(&manifest, None, &client, &InstallableLockerMock::new())
                .await;
        if let Err(ResolveError::ResolutionFailed(res_failures)) = locked_manifest {
            // A newline is added for formatting when it's a single message
            assert_eq!(
                res_failures.to_string(),
                "\nThe attr_path darwin.ps is not found for all requested systems on the same page, consider package groups with the following system groupings: (aarch64-darwin,x86_64-darwin), (x86_64-darwin)."
            );
        } else {
            panic!("expected resolution failure, got {:?}", locked_manifest);
        }
    }

    #[tokio::test]
    async fn test_locking_with_store_paths() {
        let (foo_iid, foo_descriptor, foo_locked) = fake_store_path_lock("foo");
        let store_path = &foo_descriptor.store_path;
        let system = &foo_descriptor.systems.as_ref().unwrap()[0];

        let manifest: Manifest = toml::from_str(&formatdoc! {r#"
            version = 1
            [install]
            {foo_iid}.store-path = "{store_path}"
            {foo_iid}.systems = ["{system}"]
        "#})
        .unwrap();

        let client = catalog::MockClient::new();

        let resolved_packages =
            Lockfile::resolve_manifest(&manifest, None, &client, &InstallableLockerMock::new())
                .await
                .unwrap();

        assert_eq!(&resolved_packages, &[foo_locked.into()]);
    }

    /// If a manifest doesn't have `options.systems`, it defaults to locking for
    /// 4 default systems
    #[test]
    fn collect_package_groups_defaults_to_four_systems() {
        let manifest_str = indoc! {r#"
            version = 1

            [install]
            hello_install_id.pkg-path = "hello"
        "#};
        let manifest: Manifest = toml::from_str(manifest_str).unwrap();
        let package_groups: Vec<_> = Lockfile::collect_package_groups(&manifest, None)
            .unwrap()
            .collect();

        assert_eq!(package_groups.len(), 1);

        // each system is represented by a separate package descriptor
        let systems = package_groups[0]
            .descriptors
            .iter()
            .flat_map(|d| d.systems.clone())
            .collect::<Vec<_>>();

        let expected_systems = [
            PackageSystem::Aarch64Darwin,
            PackageSystem::Aarch64Linux,
            PackageSystem::X8664Darwin,
            PackageSystem::X8664Linux,
        ];

        assert_eq!(&*systems, expected_systems.as_slice());
    }

    #[test]
    fn test_split_out_fully_locked_packages() {
        let (foo_iid, foo_descriptor, foo_locked) =
            fake_catalog_package_lock("foo", Some("group1"));
        let (bar_iid, bar_descriptor, bar_locked) =
            fake_catalog_package_lock("bar", Some("group1"));
        let (baz_iid, baz_descriptor, baz_locked) =
            fake_catalog_package_lock("baz", Some("group2"));
        let (yeet_iid, yeet_descriptor, _) = fake_catalog_package_lock("yeet", Some("group2"));

        let mut manifest = Manifest::default();
        manifest
            .install
            .inner_mut()
            .insert(foo_iid, foo_descriptor.clone());
        manifest
            .install
            .inner_mut()
            .insert(bar_iid, bar_descriptor.clone());
        manifest
            .install
            .inner_mut()
            .insert(baz_iid.clone(), baz_descriptor.clone());

        let locked = Lockfile {
            version: Version::<1>,
            manifest: manifest.clone(),
            packages: [&foo_locked, &bar_locked, &baz_locked]
                .map(|p| p.clone().into())
                .to_vec(),
            compose: None,
        };

        manifest
            .install
            .inner_mut()
            .insert(yeet_iid.clone(), yeet_descriptor.clone());

        let groups = Lockfile::collect_package_groups(&manifest, Some(&locked)).unwrap();

        let (fully_locked, to_resolve): (Vec<_>, Vec<_>) =
            Lockfile::split_fully_locked_groups(groups, Some(&locked));

        // All packages of group1 are locked
        assert_eq!(&fully_locked, &[bar_locked, foo_locked].map(Into::into));

        // Only one package of group2 is locked, so it should be in to_resolve as a group
        assert_eq!(to_resolve, vec![PackageGroup {
            name: "group2".to_string(),
            descriptors: vec![
                PackageDescriptor {
                    allow_pre_releases: None,
                    attr_path: "baz".to_string(),
                    derivation: Some(baz_locked.derivation.clone()),
                    install_id: baz_iid,
                    version: None,
                    allow_broken: None,
                    allow_insecure: None,
                    allow_unfree: None,
                    allowed_licenses: None,
                    allow_missing_builds: None,
                    systems: vec![PackageSystem::Aarch64Darwin,],
                },
                PackageDescriptor {
                    allow_pre_releases: None,
                    attr_path: "yeet".to_string(),
                    derivation: None,
                    install_id: yeet_iid,
                    version: None,
                    allow_broken: None,
                    allow_insecure: None,
                    allow_unfree: None,
                    allowed_licenses: None,
                    allow_missing_builds: None,
                    systems: vec![PackageSystem::Aarch64Darwin,],
                }
            ],
        }]);
    }

    /// When packages are locked for multiple systems,
    /// locking the same package for fewer systems should drop the extra systems
    #[test]
    fn drop_packages_for_removed_systems() {
        let (foo_iid, foo_descriptor_one_system, foo_locked) =
            fake_catalog_package_lock("foo", Some("group1"));

        let systems = &foo_descriptor_one_system
            .as_catalog_descriptor_ref()
            .expect("expected a catalog descriptor")
            .systems;

        assert_eq!(
            systems,
            &Some(vec![PackageSystem::Aarch64Darwin.to_string()]),
            "`fake_package` should set the system to [`Aarch64Darwin`]"
        );

        let mut foo_descriptor_two_systems = foo_descriptor_one_system.clone();

        if let ManifestPackageDescriptor::Catalog(descriptor) = &mut foo_descriptor_two_systems {
            descriptor
                .systems
                .as_mut()
                .unwrap()
                .push(PackageSystem::Aarch64Linux.to_string());
        } else {
            panic!("Expected a catalog descriptor");
        };

        let foo_locked_second_system = LockedPackageCatalog {
            system: PackageSystem::Aarch64Linux.to_string(),
            ..foo_locked.clone()
        };

        let mut manifest = Manifest::default();
        manifest
            .install
            .inner_mut()
            .insert(foo_iid.clone(), foo_descriptor_two_systems.clone());

        let locked = Lockfile {
            version: Version::<1>,
            manifest: manifest.clone(),
            packages: vec![
                foo_locked.clone().into(),
                foo_locked_second_system.clone().into(),
            ],
            compose: None,
        };

        manifest
            .install
            .inner_mut()
            .insert(foo_iid, foo_descriptor_one_system.clone());

        let groups = Lockfile::collect_package_groups(&manifest, Some(&locked))
            .unwrap()
            .collect::<Vec<_>>();

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].descriptors.len(), 1, "Expected only 1 descriptor");
        assert_eq!(
            groups[0].descriptors[0].systems,
            vec![PackageSystem::Aarch64Darwin,],
            "Expected only the Darwin system to be present"
        );

        let (fully_locked, to_resolve): (Vec<_>, Vec<_>) =
            Lockfile::split_fully_locked_groups(groups, Some(&locked));

        assert_eq!(fully_locked, vec![foo_locked.into()]);
        assert_eq!(to_resolve, vec![]);
    }

    /// Adding another system to a package should invalidate the entire group
    /// such that new systems are resolved with the derivation constraints
    /// of already installed systems
    #[test]
    fn invalidate_group_if_system_added() {
        let (foo_iid, foo_descriptor_one_system, foo_locked) =
            fake_catalog_package_lock("foo", Some("group1"));

        // `fake_package` sets the system to [`Aarch64Darwin`]
        let mut foo_descriptor_two_systems = foo_descriptor_one_system.clone();
        if let ManifestPackageDescriptor::Catalog(descriptor) = &mut foo_descriptor_two_systems {
            descriptor
                .systems
                .as_mut()
                .unwrap()
                .push(PackageSystem::Aarch64Linux.to_string());
        } else {
            panic!("Expected a catalog descriptor");
        };

        let mut manifest = Manifest::default();
        manifest
            .install
            .inner_mut()
            .insert(foo_iid.clone(), foo_descriptor_one_system.clone());

        let locked = Lockfile {
            version: Version::<1>,
            manifest: manifest.clone(),
            packages: vec![foo_locked.into()],
            compose: None,
        };

        manifest
            .install
            .inner_mut()
            .insert(foo_iid, foo_descriptor_two_systems.clone());

        let groups = Lockfile::collect_package_groups(&manifest, Some(&locked))
            .unwrap()
            .collect::<Vec<_>>();

        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].descriptors.len(),
            2,
            "Expected descriptors for two systems"
        );
        assert_eq!(groups[0].descriptors[0].systems, vec![
            PackageSystem::Aarch64Darwin
        ]);
        assert_eq!(groups[0].descriptors[1].systems, vec![
            PackageSystem::Aarch64Linux
        ]);

        let (fully_locked, to_resolve): (Vec<_>, Vec<_>) =
            Lockfile::split_fully_locked_groups(groups, Some(&locked));

        assert_eq!(fully_locked, vec![]);
        assert_eq!(to_resolve.len(), 1);
    }

    /// If a flake installable is already locked, it should not be resolved again.
    /// Test that the locked package and unlocked package are correctly partitioned.
    #[test]
    fn split_out_locked_installables() {
        let system = "aarch64-darwin";
        let (foo_iid, foo_descriptor, _) = fake_flake_installable_lock("foo");
        let (bar_iid, bar_descriptor, bar_locked) = fake_flake_installable_lock("bar");

        let mut manifest = Manifest::default();
        manifest.options.systems = Some(vec![system.to_string()]);

        manifest
            .install
            .inner_mut()
            .insert(foo_iid.clone(), foo_descriptor.clone().into());
        manifest
            .install
            .inner_mut()
            .insert(bar_iid.clone(), bar_descriptor.clone().into());

        let locked = Lockfile {
            version: Version::<1>,
            manifest: manifest.clone(),
            packages: vec![bar_locked.clone().into()],
            compose: None,
        };

        let flake_installables = Lockfile::collect_flake_installables(&manifest);

        let (locked, to_resolve): (Vec<_>, Vec<_>) =
            Lockfile::split_locked_flake_installables(flake_installables, Some(&locked));

        assert_eq!(locked, vec![bar_locked.into()]);
        assert_eq!(&to_resolve, &[FlakeInstallableToLock {
            install_id: foo_iid.clone(),
            descriptor: foo_descriptor.clone(),
            system: system.to_string(),
        }]);
    }

    /// If the lockfile contains a package that is not in the manifest,
    /// the lock is removed.
    #[test]
    fn remove_stale_locked_installables() {
        let system = "aarch64-darwin";
        let (_, _, bar_locked) = fake_flake_installable_lock("bar");

        let mut manifest = Manifest::default();
        manifest.options.systems = Some(vec![system.to_string()]);

        let locked = Lockfile {
            version: Version::<1>,
            manifest: manifest.clone(),
            packages: vec![bar_locked.clone().into()],
            compose: None,
        };

        let flake_installables = Lockfile::collect_flake_installables(&manifest);

        let (locked, to_resolve): (Vec<_>, Vec<_>) =
            Lockfile::split_locked_flake_installables(flake_installables, Some(&locked));

        assert_eq!(locked, vec![]);
        assert_eq!(&to_resolve, &[]);
    }

    /// If a system is removed from the manifest,
    /// the locked package for that system should be removed.
    #[test]
    fn drop_locked_installable_for_removed_systems() {
        let system = "aarch64-darwin";
        let (foo_iid, foo_descriptor, foo_locked) = fake_flake_installable_lock("foo");

        let foo_locked_system_1 = foo_locked.clone();
        let mut foo_locked_system_2 = foo_locked;
        foo_locked_system_2.locked_installable.system = PackageSystem::Aarch64Linux.to_string();

        let mut manifest = Manifest::default();
        manifest.options.systems = Some(vec![system.to_string()]);

        manifest
            .install
            .inner_mut()
            .insert(foo_iid.clone(), foo_descriptor.clone().into());

        let locked = Lockfile {
            version: Version::<1>,
            manifest: manifest.clone(),
            packages: vec![
                foo_locked_system_1.clone().into(),
                foo_locked_system_2.into(),
            ],
            compose: None,
        };

        let flake_installables = Lockfile::collect_flake_installables(&manifest);

        let (locked, to_resolve): (Vec<_>, Vec<_>) =
            Lockfile::split_locked_flake_installables(flake_installables, Some(&locked));

        assert_eq!(locked, vec![foo_locked_system_1.into()]);
        assert_eq!(&to_resolve, &[]);
    }

    /// If a system is added to the manifest, the package should be reresolved for all systems
    #[test]
    fn invalidate_locked_flake_if_system_added() {
        let system_1 = "aarch64-darwin";
        let system_2 = "aarch64-linux";
        let (foo_iid, foo_descriptor, foo_locked) = fake_flake_installable_lock("foo");

        let mut manifest = Manifest::default();
        manifest
            .install
            .inner_mut()
            .insert(foo_iid.clone(), foo_descriptor.clone().into());

        // lockfile for only system_1
        let locked = Lockfile {
            version: Version::<1>,
            manifest: manifest.clone(),
            packages: vec![foo_locked.clone().into()],
            compose: None,
        };

        // system_2 is added to the manifest
        manifest.options.systems = Some(vec![system_1.to_string(), system_2.to_string()]);

        let flake_installables = Lockfile::collect_flake_installables(&manifest);

        let (locked, to_resolve): (Vec<_>, Vec<_>) =
            Lockfile::split_locked_flake_installables(flake_installables, Some(&locked));

        assert_eq!(locked, vec![]);
        assert_eq!(
            to_resolve,
            [system_1, system_2].map(|system| FlakeInstallableToLock {
                install_id: foo_iid.clone(),
                descriptor: foo_descriptor.clone(),
                system: system.to_string(),
            })
        );
    }

    /// If all packages are already locked, return without locking/resolution
    #[tokio::test]
    async fn lock_manifest_noop_if_fully_locked() {
        let (flox, _tempdir) = flox_instance();
        let (foo_iid, foo_descriptor, foo_locked) = fake_catalog_package_lock("foo", None);
        let (bar_iid, bar_descriptor, bar_locked) = fake_flake_installable_lock("bar");

        let mut manifest = Manifest::default();
        manifest.options.systems = Some(vec![PackageSystem::Aarch64Darwin.to_string()]);
        manifest
            .install
            .inner_mut()
            .insert(foo_iid.clone(), foo_descriptor.clone());
        manifest
            .install
            .inner_mut()
            .insert(bar_iid.clone(), bar_descriptor.clone().into());

        let locked = Lockfile {
            version: Version::<1>,
            manifest: manifest.clone(),
            packages: vec![foo_locked.into(), bar_locked.into()],
            compose: None,
        };

        let locked_manifest =
            Lockfile::lock_manifest(&flox, &manifest, Some(&locked), &IncludeFetcher {
                base_directory: None,
            })
            .await
            .unwrap();

        assert_eq!(locked_manifest, locked);
    }

    proptest! {
        // This probably isn't the best suited for proptest as there are lots of
        // writes to disk.
        // 32 cases takes .1-.2 seconds for me
        #![proptest_config(ProptestConfig::with_cases(32))]
        /// If we lock twice, the second lockfile should be the same as the first
        /// Use manifests without [install] sections so we don't have to
        /// generate resolution responses
        #[test]
        fn lock_manifest_noop_if_locked_without_install_section((flox, tempdir, environments_to_include) in generate_path_environments_without_install_or_include(3)) {
            let manifest = Manifest {
                version: Version,
                include: Include {
                    environments: environments_to_include
                        .into_iter()
                        .map(|(dir, _)| IncludeDescriptor::Local {
                            dir,
                            name: None,
                        })
                        .collect(),
                },
                ..Default::default()
            };

            // Lock
            let lockfile = Lockfile::lock_manifest(&flox, &manifest, None, &IncludeFetcher {
                base_directory: Some(tempdir.path().to_path_buf()),
            })
            .block_on()
            .unwrap();

            // Lock again with a mock_include_fetcher
            let lockfile_2 =
                Lockfile::lock_manifest(&flox, &manifest, Some(&lockfile), &mock_include_fetcher())
                    .block_on()
                    .unwrap();

            prop_assert_eq!(lockfile, lockfile_2);
        }
    }

    /// If flake installables are already locked, no locking should occur.
    /// Catalog packages are still being resolved if not locked.
    #[tokio::test]
    async fn skip_flake_installables_noop_if_fully_locked() {
        let (bar_iid, bar_descriptor, bar_locked) = fake_flake_installable_lock("bar");

        let mut manifest = Manifest::default();
        manifest.options.systems = Some(vec![PackageSystem::Aarch64Darwin.to_string()]);
        manifest.install.inner_mut().insert(
            "hello".to_string(),
            ManifestPackageDescriptor::Catalog(PackageDescriptorCatalog {
                pkg_path: "hello".to_string(),
                pkg_group: None,
                priority: None,
                version: None,
                systems: None,
            }),
        );
        manifest
            .install
            .inner_mut()
            .insert(bar_iid.clone(), bar_descriptor.clone().into());

        let locked = Lockfile {
            version: Version::<1>,
            manifest: manifest.clone(),
            packages: vec![bar_locked.into()],
            compose: None,
        };

        // TODO: it would probably be better to tweak
        // fake_flake_installable_lock to return a system specific descriptor
        // and use plain hello mock
        let client_mock = auto_recording_catalog_client("hello_aarch64-darwin");

        let resolved_packages =
            Lockfile::resolve_manifest(&manifest, Some(&locked), &client_mock, &PanickingLocker)
                .await
                .unwrap();

        assert_eq!(resolved_packages.len(), 2, "{:#?}", resolved_packages);
    }

    /// If catalog packages are already locked, no locking should occur.
    /// Installables are still being resolved if not locked.
    #[tokio::test]
    async fn skip_catatalog_package_if_fully_locked() {
        let (foo_iid, foo_descriptor, foo_locked) = fake_catalog_package_lock("foo", None);
        let (bar_iid, bar_descriptor, bar_locked) = fake_flake_installable_lock("bar");

        let mut manifest = Manifest::default();
        manifest.options.systems = Some(vec![PackageSystem::Aarch64Darwin.to_string()]);
        manifest
            .install
            .inner_mut()
            .insert(foo_iid.clone(), foo_descriptor.clone());
        manifest
            .install
            .inner_mut()
            .insert(bar_iid.clone(), bar_descriptor.clone().into());

        let locked = Lockfile {
            version: Version::<1>,
            manifest: manifest.clone(),
            packages: vec![foo_locked.into()],
            compose: None,
        };

        let locker_mock = InstallableLockerMock::new();
        locker_mock.push_lock_result(Ok(bar_locked.locked_installable));

        let resolved_packages = Lockfile::resolve_manifest(
            &manifest,
            Some(&locked),
            &MockClient::default(),
            &locker_mock,
        )
        .await
        .unwrap();

        assert_eq!(resolved_packages.len(), 2, "{:#?}", resolved_packages);
    }

    /// If catalog packages are already locked, no locking should occur.
    /// Installables are still being resolved if not locked.
    #[tokio::test]
    async fn update_priority_if_fully_locked() {
        let (foo_iid, foo_descriptor, foo_locked) = fake_catalog_package_lock("foo", None);

        let mut manifest = Manifest::default();
        manifest.options.systems = Some(vec![PackageSystem::Aarch64Darwin.to_string()]);
        manifest
            .install
            .inner_mut()
            .insert(foo_iid.clone(), foo_descriptor.clone());

        let locked = Lockfile {
            version: Version::<1>,
            manifest: manifest.clone(),
            packages: vec![foo_locked.clone().into()],
            compose: None,
        };

        let mut foo_descriptor_priority_after = foo_descriptor.unwrap_catalog_descriptor().unwrap();
        foo_descriptor_priority_after.priority = Some(1);

        let mut foo_locked_priority_after = foo_locked.clone();
        foo_locked_priority_after.priority = 1;

        let mut manifest_pririty_after = manifest.clone();
        manifest_pririty_after.install.inner_mut().insert(
            foo_iid.clone(),
            foo_descriptor_priority_after.clone().into(),
        );

        let locker_mock = InstallableLockerMock::new();
        let resolved_packages = Lockfile::resolve_manifest(
            &manifest_pririty_after,
            Some(&locked),
            &MockClient::default(),
            &locker_mock,
        )
        .await
        .unwrap();

        assert_eq!(resolved_packages.as_slice(), &[
            foo_locked_priority_after.into()
        ]);
    }

    /// [Lockfile::lock_manifest] returns an error if an already
    /// locked package is no longer allowed
    #[tokio::test]
    async fn lock_manifest_catches_not_allowed_package() {
        // Create a manifest and lockfile with an unfree package foo.
        // Don't set `options.allow.unfree`
        let (foo_iid, foo_descriptor_one_system, mut foo_locked) =
            fake_catalog_package_lock("foo", None);
        foo_locked.unfree = Some(true);
        let mut manifest = Manifest::default();
        manifest
            .install
            .inner_mut()
            .insert(foo_iid.clone(), foo_descriptor_one_system.clone());

        let locked = Lockfile {
            version: Version::<1>,
            manifest: manifest.clone(),
            packages: vec![foo_locked.into()],
            compose: None,
        };

        // Set `options.allow.unfree = false` in the manifest, but not the lockfile
        manifest.options.allow.unfree = Some(false);

        let client = catalog::MockClient::new();
        assert!(matches!(
            Lockfile::resolve_manifest(
                &manifest,
                Some(&locked),
                &client,
                &InstallableLockerMock::new()
            )
            .await
            .unwrap_err(),
            ResolveError::UnfreeNotAllowed { .. }
        ));
    }

    /// [Lockfile::lock_manifest] returns an error if the server
    /// returns a package that is not allowed.
    #[tokio::test]
    async fn lock_manifest_catches_not_allowed_package_from_server() {
        let manifest = Manifest::from_str(indoc! {r#"
            version = 1

            [install]
            hello.pkg-path = "hello"

            [options]
            allow.unfree = false
        "#})
        .unwrap();

        // Return a response that says foo is unfree.
        // If this happens, it's a bug in the server, which is why we don't use
        // a mock for this test.
        let client = catalog_replay_client(
            GENERATED_DATA.join("resolve/hello_buggy_unfree_server_response.yaml"),
        )
        .await;
        assert!(matches!(
            Lockfile::resolve_manifest(&manifest, None, &client, &InstallableLockerMock::new())
                .await
                .unwrap_err(),
            ResolveError::UnfreeNotAllowed { .. }
        ));
    }

    /// [Lockfile::check_packages_are_allowed] returns an error
    /// when it finds a disallowed license
    #[test]
    fn check_packages_are_allowed_disallowed_license() {
        let (_, _, mut foo_locked) = fake_catalog_package_lock("foo", None);
        foo_locked.license = Some("disallowed".to_string());

        assert!(matches!(
            Lockfile::check_packages_are_allowed(&vec![foo_locked], &Allows {
                unfree: None,
                broken: None,
                licenses: Some(vec!["allowed".to_string()])
            }),
            Err(ResolveError::LicenseNotAllowed { .. })
        ));
    }

    /// [Lockfile::check_packages_are_allowed] does not error when
    /// a package's license is allowed
    #[test]
    fn check_packages_are_allowed_allowed_license() {
        let (_, _, mut foo_locked) = fake_catalog_package_lock("foo", None);
        foo_locked.license = Some("allowed".to_string());

        assert!(
            Lockfile::check_packages_are_allowed(&vec![foo_locked], &Allows {
                unfree: None,
                broken: None,
                licenses: Some(vec!["allowed".to_string()])
            })
            .is_ok()
        );
    }

    /// [Lockfile::check_packages_are_allowed] allows any license if allowed
    /// licenses is empty
    #[test]
    fn check_packages_are_allowed_for_empty_licenses() {
        let (_, _, mut foo_locked) = fake_catalog_package_lock("foo", None);
        foo_locked.license = Some("allowed".to_string());

        assert!(
            Lockfile::check_packages_are_allowed(&vec![foo_locked], &Allows {
                unfree: None,
                broken: None,
                licenses: Some(vec![]),
            })
            .is_ok()
        );
    }

    /// [Lockfile::check_packages_are_allowed] returns an error
    /// when a package is broken even if `allow.broken` is unset
    #[test]
    fn check_packages_are_allowed_broken_default() {
        let (_, _, mut foo_locked) = fake_catalog_package_lock("foo", None);
        foo_locked.broken = Some(true);

        assert!(matches!(
            Lockfile::check_packages_are_allowed(&vec![foo_locked], &Allows {
                unfree: None,
                broken: None,
                licenses: None
            }),
            Err(ResolveError::BrokenNotAllowed { .. })
        ));
    }

    /// [Lockfile::check_packages_are_allowed] does not error for a
    /// broken package when `allow.broken = true`
    #[test]
    fn check_packages_are_allowed_broken_true() {
        let (_, _, mut foo_locked) = fake_catalog_package_lock("foo", None);
        foo_locked.broken = Some(true);

        assert!(
            Lockfile::check_packages_are_allowed(&vec![foo_locked], &Allows {
                unfree: None,
                broken: Some(true),
                licenses: None
            })
            .is_ok()
        );
    }

    /// [Lockfile::check_packages_are_allowed] returns an error
    /// when a package is broken and `allow.broken = false`
    #[test]
    fn check_packages_are_allowed_broken_false() {
        let (_, _, mut foo_locked) = fake_catalog_package_lock("foo", None);
        foo_locked.broken = Some(true);

        assert!(matches!(
            Lockfile::check_packages_are_allowed(&vec![foo_locked], &Allows {
                unfree: None,
                broken: Some(false),
                licenses: None
            }),
            Err(ResolveError::BrokenNotAllowed { .. })
        ));
    }

    /// [Lockfile::check_packages_are_allowed] does not error for
    /// an unfree package when `allow.unfree` is unset
    #[test]
    fn check_packages_are_allowed_unfree_default() {
        let (_, _, mut foo_locked) = fake_catalog_package_lock("foo", None);
        foo_locked.unfree = Some(true);

        assert!(
            Lockfile::check_packages_are_allowed(&vec![foo_locked], &Allows {
                unfree: None,
                broken: None,
                licenses: None
            })
            .is_ok()
        );
    }

    /// [Lockfile::check_packages_are_allowed] does not error for a
    /// an unfree package when `allow.unfree = true`
    #[test]
    fn check_packages_are_allowed_unfree_true() {
        let (_, _, mut foo_locked) = fake_catalog_package_lock("foo", None);
        foo_locked.unfree = Some(true);

        assert!(
            Lockfile::check_packages_are_allowed(&vec![foo_locked], &Allows {
                unfree: Some(true),
                broken: None,
                licenses: None
            })
            .is_ok()
        );
    }

    /// [Lockfile::check_packages_are_allowed] returns an error
    /// when a package is unfree and `allow.unfree = false`
    #[test]
    fn check_packages_are_allowed_unfree_false() {
        let (_, _, mut foo_locked) = fake_catalog_package_lock("foo", None);
        foo_locked.unfree = Some(true);

        assert!(matches!(
            Lockfile::check_packages_are_allowed(&vec![foo_locked], &Allows {
                unfree: Some(false),
                broken: None,
                licenses: None
            }),
            Err(ResolveError::UnfreeNotAllowed { .. })
        ));
    }

    #[test]
    fn test_list_packages_catalog() {
        let (foo_iid, foo_descriptor, foo_locked) =
            fake_catalog_package_lock("foo", Some("group1"));
        let (bar_iid, bar_descriptor, bar_locked) =
            fake_catalog_package_lock("bar", Some("group1"));
        let (baz_iid, mut baz_descriptor, mut baz_locked) =
            fake_catalog_package_lock("baz", Some("group2"));

        if let ManifestPackageDescriptor::Catalog(ref mut descriptor) = baz_descriptor {
            descriptor.systems = Some(vec![PackageSystem::Aarch64Linux.to_string()]);
        } else {
            panic!("Expected a catalog descriptor");
        };
        baz_locked.system = PackageSystem::Aarch64Linux.to_string();

        let mut manifest = Manifest::default();
        manifest
            .install
            .inner_mut()
            .insert(foo_iid.clone(), foo_descriptor.clone());
        manifest
            .install
            .inner_mut()
            .insert(bar_iid.clone(), bar_descriptor.clone());
        manifest
            .install
            .inner_mut()
            .insert(baz_iid.clone(), baz_descriptor.clone());

        let locked = Lockfile {
            version: Version::<1>,
            manifest,
            packages: vec![
                foo_locked.clone().into(),
                bar_locked.clone().into(),
                baz_locked.clone().into(),
            ],
            compose: None,
        };

        let actual = locked
            .list_packages(&PackageSystem::Aarch64Darwin.to_string())
            .unwrap();
        let expected = [
            PackageToList::Catalog(
                foo_descriptor.unwrap_catalog_descriptor().unwrap(),
                foo_locked,
            ),
            PackageToList::Catalog(
                bar_descriptor.unwrap_catalog_descriptor().unwrap(),
                bar_locked,
            ),
            // baz is not in the list because it is not available for the requested system
        ];

        assert_eq!(&actual, &expected);
    }

    #[test]
    fn test_list_packages_flake() {
        let (foo_iid, foo_descriptor, foo_locked) = fake_flake_installable_lock("foo");
        let (baz_iid, baz_descriptor, mut baz_locked) = fake_flake_installable_lock("baz");

        baz_locked.locked_installable.system = PackageSystem::Aarch64Linux.to_string();

        let mut manifest = Manifest::default();
        manifest
            .install
            .inner_mut()
            .insert(foo_iid.clone(), foo_descriptor.clone().into());
        manifest
            .install
            .inner_mut()
            .insert(baz_iid.clone(), baz_descriptor.into());

        let locked = Lockfile {
            version: Version::<1>,
            manifest,
            packages: vec![foo_locked.clone().into(), baz_locked.clone().into()],
            compose: None,
        };

        let actual = locked
            .list_packages(&PackageSystem::Aarch64Darwin.to_string())
            .unwrap();
        let expected = [
            PackageToList::Flake(foo_descriptor, foo_locked), // baz is not in the list because it is not available for the requested system
        ];

        assert_eq!(&actual, &expected);
    }

    #[test]
    fn test_list_packages_store_path() {
        let (foo_iid, foo_descriptor, foo_locked) = fake_store_path_lock("foo");
        let (baz_iid, baz_descriptor, mut baz_locked) = fake_store_path_lock("baz");

        baz_locked.system = PackageSystem::Aarch64Linux.to_string();

        let mut manifest = Manifest::default();
        manifest
            .install
            .inner_mut()
            .insert(foo_iid.clone(), foo_descriptor.clone().into());
        manifest
            .install
            .inner_mut()
            .insert(baz_iid.clone(), baz_descriptor.into());

        let locked = Lockfile {
            version: Version::<1>,
            manifest,
            packages: vec![foo_locked.clone().into(), baz_locked.clone().into()],
            compose: None,
        };

        let actual = locked
            .list_packages(&PackageSystem::Aarch64Darwin.to_string())
            .unwrap();
        let expected = [
            PackageToList::StorePath(foo_locked), // baz is not in the list because it is not available for the requested system
        ];

        assert_eq!(&actual, &expected);
    }

    #[test]
    fn respects_flake_descriptor_systems() {
        let manifest_contents = formatdoc! {r#"
        version = 1

        [install]
        bpftrace.flake = "github:NixOS/nixpkgs#bpftrace"
        bpftrace.systems = ["x86_64-linux"]

        [options]
        systems = ["aarch64-linux", "x86_64-linux"]
        "#};
        let manifest = toml_edit::de::from_str(&manifest_contents).unwrap();
        let installables = Lockfile::collect_flake_installables(&manifest).collect::<Vec<_>>();
        assert_eq!(installables.len(), 1);
        assert_eq!(installables[0].system.as_str(), "x86_64-linux");
    }

    /// [Lockfile::merge_manifest] fetches an included environment when it is
    /// not already locked
    #[test]
    fn merge_manifest_fetches_included_environment() {
        let (flox, tempdir) = flox_instance();
        let manifest_contents = indoc! {r#"
        version = 1

        [include]
        environments = [
          { dir = "dep1" }
        ]
        "#};
        let manifest = toml_edit::de::from_str(manifest_contents).unwrap();

        // Create dep1 environment
        let dep1_path = tempdir.path().join("dep1");
        let dep1_manifest_contents = indoc! {r#"
        version = 1

        [vars]
        foo = "dep1"
        "#};

        fs::create_dir(&dep1_path).unwrap();
        let mut dep1 = new_path_environment_in(&flox, dep1_manifest_contents, &dep1_path);
        dep1.lockfile(&flox).unwrap();

        // Merge
        let (merged, compose) = Lockfile::merge_manifest(
            &flox,
            &manifest,
            None,
            &IncludeFetcher {
                base_directory: Some(tempdir.path().to_path_buf()),
            },
            ManifestMerger::Shallow(ShallowMerger),
            None,
        )
        .unwrap();

        assert_eq!(merged, Manifest {
            version: Version,
            vars: Vars(BTreeMap::from([("foo".to_string(), "dep1".to_string())])),
            ..Default::default()
        });
        assert_eq!(
            compose.unwrap().include[0].manifest,
            toml_edit::de::from_str(dep1_manifest_contents).unwrap()
        )
    }

    /// [Lockfile::merge_manifest] preserves precedence of includes
    #[test]
    fn merge_manifest_preserves_include_order() {
        let (flox, tempdir) = flox_instance();
        let manifest_contents = indoc! {r#"
        version = 1

        [include]
        environments = [
          { dir = "lowest_precedence" },
          { dir = "higher_precedence" }
        ]

        [vars]
        foo = "highest_precedence"
        "#};
        let manifest = toml_edit::de::from_str(manifest_contents).unwrap();

        // Create lowest_precedence environment
        let lowest_precedence_path = tempdir.path().join("lowest_precedence");
        let lowest_precedence_manifest_contents = indoc! {r#"
        version = 1

        [vars]
        foo = "lowest_precedence"
        bar = "lowest_precedence"
        "#};
        fs::create_dir(&lowest_precedence_path).unwrap();
        let mut lowest_precedence = new_path_environment_in(
            &flox,
            lowest_precedence_manifest_contents,
            &lowest_precedence_path,
        );
        lowest_precedence.lockfile(&flox).unwrap();

        // Create higher precedence environment
        let higher_precedence_path = tempdir.path().join("higher_precedence");
        let higher_precedence_manifest_contents = indoc! {r#"
        version = 1

        [vars]
        foo = "higher_precedence"
        bar = "higher_precedence"
        "#};
        fs::create_dir(&higher_precedence_path).unwrap();
        let mut higher_precedence = new_path_environment_in(
            &flox,
            higher_precedence_manifest_contents,
            &higher_precedence_path,
        );
        higher_precedence.lockfile(&flox).unwrap();

        // Merge
        let (merged, compose) = Lockfile::merge_manifest(
            &flox,
            &manifest,
            None,
            &IncludeFetcher {
                base_directory: Some(tempdir.path().to_path_buf()),
            },
            ManifestMerger::Shallow(ShallowMerger),
            None,
        )
        .unwrap();

        assert_eq!(merged, Manifest {
            version: Version,
            vars: Vars(BTreeMap::from([
                ("foo".to_string(), "highest_precedence".to_string()),
                ("bar".to_string(), "higher_precedence".to_string())
            ])),
            ..Default::default()
        });
        assert_eq!(
            compose.as_ref().unwrap().include[0].manifest,
            toml_edit::de::from_str(lowest_precedence_manifest_contents).unwrap()
        );
        assert_eq!(
            compose.unwrap().include[1].manifest,
            toml_edit::de::from_str(higher_precedence_manifest_contents).unwrap()
        );
    }

    /// Skipping fetching an already fetched environment shouldn't break
    /// precedence.
    ///
    /// Suppose an environment starts out with a single included environment,
    /// middle_precedence,
    /// and the environment is merged.
    /// Then suppose two other environments lowest_precedence and
    /// highest_precedence are added.
    /// Precedence should still reflect the order of included environments.
    #[tokio::test]
    async fn merge_manifest_respects_precedence_when_skipping_fetch() {
        let (flox, tempdir) = flox_instance();

        let manifest_contents = indoc! {r#"
        version = 1

        [include]
        environments = [
          { dir = "middle_precedence" }
        ]
        "#};
        let manifest = toml_edit::de::from_str(manifest_contents).unwrap();

        // Create middle_precedence environment
        let middle_precedence_path = tempdir.path().join("middle_precedence");
        let middle_precedence_manifest_contents = indoc! {r#"
        version = 1

        [vars]
        foo = "middle_precedence"
        "#};
        fs::create_dir(&middle_precedence_path).unwrap();
        let mut middle_precedence = new_path_environment_in(
            &flox,
            middle_precedence_manifest_contents,
            &middle_precedence_path,
        );
        middle_precedence.lockfile(&flox).unwrap();

        // Lock
        let include_fetcher = IncludeFetcher {
            base_directory: Some(tempdir.path().to_path_buf()),
        };

        let lockfile = Lockfile::lock_manifest(&flox, &manifest, None, &include_fetcher)
            .await
            .unwrap();

        // Edit manifest to include two more includes
        let manifest_contents = indoc! {r#"
        version = 1

        [include]
        environments = [
          { dir = "lowest_precedence" },
          { dir = "middle_precedence" },
          { dir = "highest_precedence" },
        ]
        "#};
        let manifest = toml_edit::de::from_str(manifest_contents).unwrap();

        // Create lowest_precedence environment
        let lowest_precedence_path = tempdir.path().join("lowest_precedence");
        let lowest_precedence_manifest_contents = indoc! {r#"
        version = 1

        [vars]
        foo = "lowest_precedence"
        "#};
        fs::create_dir(&lowest_precedence_path).unwrap();
        let mut lowest_precedence = new_path_environment_in(
            &flox,
            lowest_precedence_manifest_contents,
            &lowest_precedence_path,
        );
        lowest_precedence.lockfile(&flox).unwrap();

        // Create highest_precedence environment
        let highest_precedence_path = tempdir.path().join("highest_precedence");
        let highest_precedence_manifest_contents = indoc! {r#"
        version = 1

        [vars]
        foo = "highest_precedence"
        "#};
        fs::create_dir(&highest_precedence_path).unwrap();
        let mut highest_precedence = new_path_environment_in(
            &flox,
            highest_precedence_manifest_contents,
            &highest_precedence_path,
        );
        highest_precedence.lockfile(&flox).unwrap();

        // Merge
        let (merged, compose) = Lockfile::merge_manifest(
            &flox,
            &manifest,
            Some(&lockfile),
            &include_fetcher,
            ManifestMerger::Shallow(ShallowMerger),
            None,
        )
        .unwrap();

        assert_eq!(merged, Manifest {
            version: Version,
            vars: Vars(BTreeMap::from([(
                "foo".to_string(),
                "highest_precedence".to_string()
            ),])),
            ..Default::default()
        });
        assert_eq!(
            compose.as_ref().unwrap().include[0].manifest,
            toml_edit::de::from_str(lowest_precedence_manifest_contents).unwrap()
        );
        assert_eq!(
            compose.as_ref().unwrap().include[1].manifest,
            toml_edit::de::from_str(middle_precedence_manifest_contents).unwrap()
        );
        assert_eq!(
            compose.as_ref().unwrap().include[2].manifest,
            toml_edit::de::from_str(highest_precedence_manifest_contents).unwrap()
        );
    }

    /// Re-merge after editing an included environment
    /// If modify_include_descriptor is true, modify the include descriptor
    /// which should trigger a re-fetch.
    /// Otherwise, re-merging should not re-fetch.
    async fn re_merge_after_editing_dep(modify_include_descriptor: bool) {
        let (flox, tempdir) = flox_instance();

        let mut manifest_contents = indoc! {r#"
        version = 1

        [include]
        environments = [
          { dir = "dep1" }
        ]
        "#};
        let mut manifest = toml_edit::de::from_str(manifest_contents).unwrap();

        // Create dep1 environment
        let dep1_path = tempdir.path().join("dep1");
        let dep1_manifest_contents = indoc! {r#"
        version = 1

        [vars]
        foo = "dep1"
        "#};
        let dep1_manifest = toml_edit::de::from_str(dep1_manifest_contents).unwrap();

        fs::create_dir(&dep1_path).unwrap();
        let mut dep1 = new_path_environment_in(&flox, dep1_manifest_contents, &dep1_path);
        dep1.lockfile(&flox).unwrap();

        // Lock
        let include_fetcher = IncludeFetcher {
            base_directory: Some(tempdir.path().to_path_buf()),
        };

        let lockfile = Lockfile::lock_manifest(&flox, &manifest, None, &include_fetcher)
            .await
            .unwrap();

        assert_eq!(lockfile.manifest, Manifest {
            version: Version,
            vars: Vars(BTreeMap::from([("foo".to_string(), "dep1".to_string())])),
            ..Default::default()
        });
        assert_eq!(
            lockfile.compose.as_ref().unwrap().include[0].manifest,
            toml_edit::de::from_str(dep1_manifest_contents).unwrap()
        );

        // Edit dep1 and then change its name in the include descriptor and re-merge
        let dep1_edited_manifest_contents = indoc! {r#"
        version = 1

        [vars]
        foo = "dep1 edited"
        "#};
        let dep1_edited_manifest = toml_edit::de::from_str(dep1_edited_manifest_contents).unwrap();

        dep1.edit(&flox, dep1_edited_manifest_contents.to_string())
            .unwrap();

        if modify_include_descriptor {
            manifest_contents = indoc! {r#"
            version = 1

            [include]
            environments = [
              { dir = "dep1", name = "dep1 edited" }
            ]
            "#};
            manifest = toml_edit::de::from_str(manifest_contents).unwrap();
        }

        // Merge
        let (merged, compose) = Lockfile::merge_manifest(
            &flox,
            &manifest,
            Some(&lockfile),
            &include_fetcher,
            ManifestMerger::Shallow(ShallowMerger),
            None,
        )
        .unwrap();

        assert_eq!(merged, Manifest {
            version: Version,
            vars: Vars(BTreeMap::from([(
                "foo".to_string(),
                if modify_include_descriptor {
                    "dep1 edited".to_string()
                } else {
                    "dep1".to_string()
                }
            )])),
            ..Default::default()
        });
        assert_eq!(
            compose.unwrap().include[0].manifest,
            if modify_include_descriptor {
                dep1_edited_manifest
            } else {
                dep1_manifest
            }
        );
    }

    /// If included environments have already been locked, the existing locked include should be used
    #[tokio::test]
    async fn merge_manifest_does_not_refetch_if_include_descriptor_unchanged() {
        re_merge_after_editing_dep(false).await;
    }

    /// [Lockfile::merge_manifest] re-fetches if any part of an include
    /// descriptor has changed
    #[tokio::test]
    async fn merge_manifest_refetches_if_include_descriptor_changed() {
        re_merge_after_editing_dep(true).await;
    }

    /// [Lockfile::merge_manifest] doesn't leave stale locked includes
    #[tokio::test]
    async fn merge_manifest_removes_stale_locked_includes() {
        let (flox, tempdir) = flox_instance();

        let mut manifest_contents = indoc! {r#"
        version = 1

        [include]
        environments = [
          { dir = "dep1" }
        ]
        "#};
        let mut manifest = toml_edit::de::from_str(manifest_contents).unwrap();

        // Create dep1 environment
        let dep1_path = tempdir.path().join("dep1");
        let dep1_manifest_contents = indoc! {r#"
        version = 1

        [vars]
        foo = "dep1"
        "#};
        let dep1_manifest = toml_edit::de::from_str(dep1_manifest_contents).unwrap();

        fs::create_dir(&dep1_path).unwrap();
        let mut dep1 = new_path_environment_in(&flox, dep1_manifest_contents, &dep1_path);
        dep1.lockfile(&flox).unwrap();

        // Lock
        let include_fetcher = IncludeFetcher {
            base_directory: Some(tempdir.path().to_path_buf()),
        };

        let lockfile = Lockfile::lock_manifest(&flox, &manifest, None, &include_fetcher)
            .await
            .unwrap();

        assert_eq!(lockfile.manifest, Manifest {
            version: Version,
            vars: Vars(BTreeMap::from([("foo".to_string(), "dep1".to_string())])),
            ..Default::default()
        });
        assert_eq!(
            lockfile.compose.as_ref().unwrap().include[0].manifest,
            dep1_manifest,
        );

        // Remove the include of dep1
        manifest_contents = indoc! {r#"
        version = 1
        "#};
        manifest = toml_edit::de::from_str(manifest_contents).unwrap();

        // Merge
        let (merged, compose) = Lockfile::merge_manifest(
            &flox,
            &manifest,
            Some(&lockfile),
            &include_fetcher,
            ManifestMerger::Shallow(ShallowMerger),
            None,
        )
        .unwrap();

        assert_eq!(merged, manifest);

        assert!(compose.is_none());
    }

    /// [Lockfile::merge_manifest] errors if locked include names are not unique
    #[test]
    fn merge_manifest_errors_for_non_unique_include_names() {
        let (flox, tempdir) = flox_instance();

        let manifest_contents = indoc! {r#"
        version = 1

        [include]
        environments = [
          { dir = "dep1" },
          { dir = "dep2" }
        ]
        "#};
        let manifest = toml_edit::de::from_str(manifest_contents).unwrap();

        // Create dep1 named dep
        let dep1_path = tempdir.path().join("dep1");
        let dep1_manifest_contents = indoc! {r#"
        version = 1
        "#};
        fs::create_dir(&dep1_path).unwrap();
        let mut dep1 =
            new_named_path_environment_in(&flox, dep1_manifest_contents, &dep1_path, "dep");
        dep1.lockfile(&flox).unwrap();

        // Create dep2 named dep
        let dep2_path = tempdir.path().join("dep2");
        let dep2_manifest_contents = indoc! {r#"
        version = 1
        "#};
        fs::create_dir(&dep2_path).unwrap();
        let mut dep2 =
            new_named_path_environment_in(&flox, dep2_manifest_contents, &dep2_path, "dep");
        dep2.lockfile(&flox).unwrap();

        // Merge
        let include_fetcher = IncludeFetcher {
            base_directory: Some(tempdir.path().to_path_buf()),
        };

        let err = Lockfile::merge_manifest(
            &flox,
            &manifest,
            None,
            &include_fetcher,
            ManifestMerger::Shallow(ShallowMerger),
            None,
        )
        .unwrap_err();

        let RecoverableMergeError::Catchall(message) = err else {
            panic!();
        };
        assert_eq!(
            message,
            indoc! {"multiple environments in include.environments have the name 'dep'
            A unique name can be provided with the 'name' field."}
        );
    }

    #[test]
    fn merge_manifest_uses_the_merged_manifests_of_includes() {
        let env_ref = EnvironmentRef::new("owner", "name").unwrap();
        let (flox, _tempdir) = flox_instance_with_optional_floxhub(Some(env_ref.owner()));

        // Create two "child" environments, local and remote.
        let mut dep_child_local = new_path_environment(&flox, indoc! {r#"
                version = 1

                [vars]
                child_local = "hi"
            "#});
        dep_child_local.lockfile(&flox).unwrap();

        let _dep_child_remote = mock_remote_environment(
            &flox,
            indoc! {r#"
                version = 1

                [vars]
                child_remote = "hi"
            "#},
            env_ref.owner().clone(),
            Some(&env_ref.name().to_string()),
        );

        // Create a "parent" environment that includes the local and remote "children".
        let mut dep_parent = new_path_environment(&flox, &formatdoc! {r#"
                version = 1

                [vars]
                parent = "hi"

                [include]
                environments = [
                    {{ dir = "{include_path}" }},
                    {{ remote = "owner/name" }},
                ]
            "#, include_path = dep_child_local.parent_path().unwrap().to_string_lossy() });
        dep_parent.lockfile(&flox).unwrap();

        // Create a composer environment that indirectly includes the "children".
        let mut composer = new_path_environment(&flox, &formatdoc! {r#"
                version = 1

                [vars]
                composer = "hi"

                [include]
                environments = [
                    {{ dir = "{include_path}" }},
                ]
            "#, include_path = &dep_parent.parent_path().unwrap().to_string_lossy() });

        let lockfile: Lockfile = composer.lockfile(&flox).unwrap().into();
        assert_eq!(
            lockfile.manifest,
            Manifest {
                vars: Vars(BTreeMap::from([
                    ("child_local".to_string(), "hi".to_string()),
                    ("child_remote".to_string(), "hi".to_string()),
                    ("parent".to_string(), "hi".to_string()),
                    ("composer".to_string(), "hi".to_string()),
                ])),
                ..Default::default()
            },
            "composer should include fields from both indirect child includes"
        )
    }
}
