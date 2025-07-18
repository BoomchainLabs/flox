use std::path::{Path, PathBuf};
use std::time::SystemTime;

use flox_core::{SerializeError, Version, serialize_atomically};
use fslock::LockFile;
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument};

use super::environment::{EnvironmentPointer, path_hash};
use super::floxmeta::{FloxMeta, FloxMetaError};
use crate::data::CanonicalPath;
use crate::flox::Flox;
use crate::utils::logging::traceable_path;

pub const ENV_REGISTRY_FILENAME: &str = "env-registry.json";

/// Errors encountered while interacting with the environment registry.
#[derive(Debug, thiserror::Error)]
pub enum EnvRegistryError {
    #[error("couldn't acquire environment registry file lock")]
    AcquireLock(#[source] fslock::Error),
    #[error("couldn't read environment registry file")]
    ReadRegistry(#[source] std::io::Error),
    #[error("couldn't parse environment registry")]
    ParseRegistry(#[source] serde_json::Error),
    #[error("no environments registered with key: {0}")]
    UnknownKey(String),
    #[error("did not find environment in registry")]
    EnvNotRegistered,
    #[error("failed to write environment registry file")]
    WriteEnvironmentRegistry(#[source] SerializeError),
    #[error("no registry found")]
    NoEnvRegistry,
    #[error(transparent)]
    FloxMeta(#[from] FloxMetaError),
}

/// A local registry of environments on the system.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary, PartialEq))]
pub struct EnvRegistry {
    /// The schema version of the local environment registry file.
    pub version: Version<1>,
    /// The list of locations at which environments can be found and the metadata about
    /// the environments that have existed there.
    // Note: We use this ugly macro to generate fewer `RegistryEntry`s than `proptest`s default,
    //       and this makes a _huge_ difference in test execution speed.
    #[cfg_attr(
        test,
        proptest(
            strategy = "proptest::collection::vec(proptest::arbitrary::any::<RegistryEntry>(), 0..=3)"
        )
    )]
    pub entries: Vec<RegistryEntry>,
}

impl EnvRegistry {
    /// Returns the [RegistryEntry] that corresponds to the provided path hash, if it exists.
    pub fn entry_for_hash_mut(&mut self, hash: &str) -> Option<&mut RegistryEntry> {
        self.entries
            .iter_mut()
            .find(|entry| entry.path_hash == hash)
    }

    /// Returns the [RegistryEntry] that corresponds to the provided path hash, if it exists.
    pub fn entry_for_hash(&self, hash: &str) -> Option<&RegistryEntry> {
        self.entries.iter().find(|entry| entry.path_hash == hash)
    }

    /// Returns the path associated with a particular hash
    pub fn path_for_hash(&self, hash: &str) -> Result<PathBuf, EnvRegistryError> {
        let entry = self
            .entry_for_hash(hash)
            .ok_or(EnvRegistryError::EnvNotRegistered)?;
        Ok(entry.path.clone())
    }

    /// Registers the environment, creating a new [RegistryEntry] if necessary and returning the
    /// [RegisteredEnv] that was created. If the environment was already it returns `Ok(None)`.
    fn register_env(
        &mut self,
        dot_flox_path: &impl AsRef<Path>,
        hash: &str,
        env_pointer: &EnvironmentPointer,
    ) -> Result<Option<RegisteredEnv>, EnvRegistryError> {
        let entry = match self.entry_for_hash_mut(hash) {
            Some(entry) => entry,
            None => {
                self.entries.push(RegistryEntry {
                    path_hash: hash.to_string(),
                    path: dot_flox_path.as_ref().to_path_buf(),
                    envs: vec![],
                });
                self.entries
                    .last_mut()
                    .expect("didn't find registry entry that was just pushed")
            },
        };
        entry.register_env(env_pointer)
    }

    /// Deregisters and returns the latest entry if it is the same type of environment and has
    /// the same pointer.
    fn deregister_env(
        &mut self,
        hash: &str,
        env_pointer: &EnvironmentPointer,
    ) -> Result<RegisteredEnv, EnvRegistryError> {
        let entry = self
            .entry_for_hash_mut(hash)
            .ok_or(EnvRegistryError::UnknownKey(hash.to_string()))?;
        let res = entry
            .deregister_env(env_pointer)
            .ok_or(EnvRegistryError::EnvNotRegistered);
        // Remove the entry if it's empty. We use [Vec::retain] because the entry doesn't
        // track its own index.
        if entry.envs.is_empty() {
            self.entries.retain(|e| e.path_hash != hash);
        }
        res
    }

    /// Prunes environments that no longer exist on disk from the Registry and FloxMeta.
    fn prune_nonexistent(&mut self, flox: &Flox) -> Result<(), EnvRegistryError> {
        self.entries
            .iter()
            .filter(|entry| !entry.exists())
            .try_for_each(|entry| {
                for env in entry.envs.iter() {
                    // Prune floxmeta branches for managed environments
                    if let EnvironmentPointer::Managed(ref pointer) = env.pointer {
                        // Previously canonicalized path that we know no longer exists.
                        let path = CanonicalPath::new_unchecked(&entry.path);
                        let floxmeta = FloxMeta::open(flox, pointer)?;
                        floxmeta.prune_branches(pointer, &path)?;
                    }
                }

                Ok(())
            })
            .map_err(EnvRegistryError::FloxMeta)?;

        // The environment registry is the only method we have of determining
        // whether a branch in floxmeta should be garbage collected, so only
        // remove entries after pruning floxmeta
        self.entries.retain(|entry| entry.exists());

        Ok(())
    }
}

/// Metadata about the location at which one or more environments were registered over time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegistryEntry {
    /// The truncated hash of the path to the environment.
    #[serde(rename = "hash")]
    pub path_hash: String,
    /// The path to the environment's `.flox` directory
    pub path: PathBuf,
    /// The list of environments that have existed at this path
    /// since the last time environments were garbage collected.
    pub envs: Vec<RegisteredEnv>,
}

impl RegistryEntry {
    /// Returns true if the `.flox` path still exists on disk.
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Returns the latest environment registered at this location.
    pub fn latest_env(&self) -> Option<&RegisteredEnv> {
        self.envs.iter().last()
    }

    /// Adds the environment to the list of registered environments. This is a no-op if the latest
    /// registered environment has the same environment pointer, which indicates that it's the
    /// currently registered environment.
    fn register_env(
        &mut self,
        env_pointer: &EnvironmentPointer,
    ) -> Result<Option<RegisteredEnv>, EnvRegistryError> {
        // Bail early if the environment is the same as the latest registered one
        if self.is_same_as_latest_env(env_pointer) {
            return Ok(None);
        }

        // [SystemTime] isn't guaranteed to be monotonic, so we account for that by setting the
        // `created_at` time to be `max(now, latest_created_at)`.
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("'now' was earlier than UNIX epoch")
            .as_secs();
        let created_at = if let Some(RegisteredEnv { created_at, .. }) = self.latest_env() {
            now.max(*created_at)
        } else {
            now
        };

        let env = RegisteredEnv {
            created_at,
            pointer: env_pointer.clone(),
        };
        self.envs.push(env.clone());
        Ok(Some(env))
    }

    /// Returns true if there is a latest registered environment that is a managed environment with
    /// the same
    pub fn is_same_as_latest_env(&self, ptr: &EnvironmentPointer) -> bool {
        if let Some(RegisteredEnv { pointer, .. }) = self.latest_env() {
            return pointer == ptr;
        }
        false
    }

    /// Deregisters and returns the latest entry if it is the same type of environment and has
    /// the same pointer.
    fn deregister_env(&mut self, ptr: &EnvironmentPointer) -> Option<RegisteredEnv> {
        if self.is_same_as_latest_env(ptr) {
            return Some(self.envs.pop().expect("envs was assumed to be non-empty"));
        }
        None
    }
}

/// Metadata about an environment that has been registered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub struct RegisteredEnv {
    /// The time at which this environment was registered in seconds since the Unix Epoch.
    pub created_at: u64,
    /// The metadata about the owner and name of the environment if this environment is a
    /// managed environment.
    pub pointer: EnvironmentPointer,
}

/// Returns the path to the user's environment registry file.
pub fn env_registry_path(flox: &Flox) -> PathBuf {
    flox.data_dir.join(ENV_REGISTRY_FILENAME)
}

/// Returns the path to the user's environment registry lock file. The presensce
/// of the lock file does not indicate an active lock because the file isn't
/// removed after use. This is a separate file because we replace the registry
/// on write.
fn env_registry_lock_path(reg_path: impl AsRef<Path>) -> PathBuf {
    reg_path.as_ref().with_extension("lock")
}

/// Returns the parsed environment registry file or `None` if it doesn't yet exist.
pub fn read_environment_registry(
    path: impl AsRef<Path>,
) -> Result<Option<EnvRegistry>, EnvRegistryError> {
    let path = path.as_ref();
    if !path.exists() {
        debug!(
            path = traceable_path(&path),
            "environment registry not found"
        );
        return Ok(None);
    }
    let contents = std::fs::read_to_string(path).map_err(EnvRegistryError::ReadRegistry)?;
    let parsed: EnvRegistry =
        serde_json::from_str(&contents).map_err(EnvRegistryError::ParseRegistry)?;
    Ok(Some(parsed))
}

/// Writes the environment registry to disk.
///
/// First the registry is written to a temporary file and then it is renamed so the write appears
/// atomic. This also takes a [LockFile] argument to ensure that the write can only be performed
/// when the lock is acquired. It is a bug if you pass a [LockFile] that doesn't correspond to the
/// environment registry, as that is essentially bypassing the lock.
fn write_environment_registry(
    reg: &EnvRegistry,
    reg_path: impl AsRef<Path>,
    _lock: LockFile,
) -> Result<(), EnvRegistryError> {
    serialize_atomically(reg, &reg_path, _lock).map_err(EnvRegistryError::WriteEnvironmentRegistry)
}

/// Acquires the filesystem-based lock on the user's environment registry file
fn acquire_env_registry_lock(reg_path: impl AsRef<Path>) -> Result<LockFile, EnvRegistryError> {
    let lock_path = env_registry_lock_path(reg_path);
    let mut lock = LockFile::open(lock_path.as_os_str()).map_err(EnvRegistryError::AcquireLock)?;
    lock.lock().map_err(EnvRegistryError::AcquireLock)?;
    Ok(lock)
}

/// Ensures that the environment is registered. This is a no-op if it is already registered.
pub fn ensure_registered(
    flox: &Flox,
    dot_flox_path: &CanonicalPath,
    env_pointer: &EnvironmentPointer,
) -> Result<(), EnvRegistryError> {
    // Acquire the lock before reading the registry so that we know there are no modifications while
    // we're editing it.
    let reg_path = env_registry_path(flox);
    let lock = acquire_env_registry_lock(&reg_path)?;
    let mut reg = read_environment_registry(&reg_path)?.unwrap_or_default();
    let dot_flox_hash = path_hash(dot_flox_path);
    // Skip writing the registry if the environment was already registered
    if reg
        .register_env(dot_flox_path, &dot_flox_hash, env_pointer)?
        .is_some()
    {
        write_environment_registry(&reg, &reg_path, lock)?;
    }
    Ok(())
}

/// Deletes the environment from the registry.
///
/// The deleted environment must be of the same type as the requested environment as indicated by
/// the presence of a [ManagedPointer].
pub fn deregister(
    flox: &Flox,
    dot_flox_path: &CanonicalPath,
    env_pointer: &EnvironmentPointer,
) -> Result<(), EnvRegistryError> {
    // Acquire the lock before reading the registry so that we know there are no modifications while
    // we're editing it.
    let reg_path = env_registry_path(flox);
    let lock = acquire_env_registry_lock(&reg_path)?;
    let mut reg = read_environment_registry(&reg_path)?.unwrap_or_default();
    let dot_flox_hash = path_hash(dot_flox_path);
    reg.deregister_env(&dot_flox_hash, env_pointer)?;
    write_environment_registry(&reg, &reg_path, lock)?;
    Ok(())
}

/// Garbage collect non-existent environments from the registry. Writes to the
/// registry file, in addition to returning the updated registry to avoid a
/// second read by any consumers.
#[instrument(skip_all, fields(progress = "Garbage collecting stale environments"))]
pub fn garbage_collect(flox: &Flox) -> Result<EnvRegistry, EnvRegistryError> {
    let reg_path = env_registry_path(flox);
    let lock = acquire_env_registry_lock(&reg_path)?;
    let mut reg = read_environment_registry(&reg_path)?.ok_or(EnvRegistryError::NoEnvRegistry)?;
    reg.prune_nonexistent(flox)?;
    write_environment_registry(&reg, &reg_path, lock)?;
    Ok(reg)
}

#[cfg(test)]
mod test {
    use std::fs::OpenOptions;
    use std::io::BufWriter;

    use proptest::arbitrary::{Arbitrary, any};
    use proptest::collection::vec;
    use proptest::path::PathParams;
    use proptest::strategy::{BoxedStrategy, Just, Strategy};
    use proptest::{prop_assert, prop_assert_eq, prop_assume, proptest};
    use tempfile::tempdir;

    use super::*;
    use crate::flox::test_helpers::flox_instance;
    use crate::models::environment::path_environment::test_helpers::new_path_environment;

    impl Arbitrary for RegistryEntry {
        type Parameters = ();
        type Strategy = BoxedStrategy<Self>;

        fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
            // Creates a RegistryEntry with the following guarantees:
            // - The hash is the actual hash of the path, though the path may not exist.
            // - The registered envs are sorted in ascending order by `created_at`, as they would
            //   be in reality.
            (
                PathBuf::arbitrary_with(PathParams::default().with_components(1..3)),
                vec(any::<RegisteredEnv>(), 0..=3),
            )
                .prop_flat_map(|(path, mut registered_envs)| {
                    registered_envs.sort_by_cached_key(|e| e.created_at);
                    (
                        Just(path.clone()),
                        Just(path_hash(&path)),
                        Just(registered_envs),
                    )
                })
                .prop_map(|(path, hash, envs)| RegistryEntry {
                    path_hash: hash.to_string(),
                    path,
                    envs,
                })
                .boxed()
        }
    }

    proptest! {
        #[test]
        fn can_roundtrip(reg: EnvRegistry) {
            let serialized = serde_json::to_string(&reg).unwrap();
            let deserialized = serde_json::from_str::<EnvRegistry>(&serialized).unwrap();
            prop_assert_eq!(reg, deserialized);
        }

        #[test]
        fn reads_registry(reg: EnvRegistry) {
            let tmp_dir = tempdir().unwrap();
            let reg_path = tmp_dir.path().join(ENV_REGISTRY_FILENAME);
            let file = OpenOptions::new().write(true).create_new(true).open(&reg_path).unwrap();
            let writer = BufWriter::new(file);
            serde_json::to_writer(writer, &reg).unwrap();
            let reg_read = read_environment_registry(&reg_path).unwrap().unwrap();
            prop_assert_eq!(reg, reg_read);
        }

        #[test]
        fn writes_registry(reg: EnvRegistry) {
            let (flox, _temp_dir_handle) = flox_instance();
            let reg_path = env_registry_path(&flox);
            let lock_path = env_registry_lock_path(&reg_path);
            let lock = LockFile::open(&lock_path).unwrap();
            prop_assert!(!reg_path.exists());
            write_environment_registry(&reg, &reg_path, lock).unwrap();
            prop_assert!(reg_path.exists());
        }

        #[test]
        fn new_env_added_to_reg_entry(mut entry: RegistryEntry, ptr: EnvironmentPointer) {
            // Skip cases where they're the same since that's a no-op
            prop_assume!(!entry.is_same_as_latest_env(&ptr));
            let previous_envs = entry.envs.clone();
            entry.register_env(&ptr).unwrap();
            let new_envs = entry.envs;
            prop_assert!(new_envs.len() == previous_envs.len() + 1);
            let latest_ptr = new_envs.into_iter().next_back().unwrap().pointer;
            prop_assert_eq!(latest_ptr, ptr);
        }

        #[test]
        fn noop_on_existing_env(mut entry: RegistryEntry, ptr: EnvironmentPointer) {
            if entry.is_same_as_latest_env(&ptr) {
                let previous_envs = entry.envs.clone();
                entry.register_env(&ptr).unwrap();
                let new_envs = entry.envs.clone();
                prop_assert_eq!(previous_envs, new_envs);
            } else {
                entry.register_env(&ptr).unwrap();
                let previous_envs = entry.envs.clone();
                entry.register_env(&ptr).unwrap();
                let new_envs = entry.envs.clone();
                prop_assert_eq!(previous_envs, new_envs);
            }
        }

        #[test]
        fn none_on_nonexistent_registry_file(path: PathBuf) {
            prop_assume!(path != PathBuf::from(""));
            prop_assume!(!path.exists() || path.is_file());
            prop_assert!(read_environment_registry(path).unwrap().is_none())
        }

        #[test]
        fn ensures_new_registration(existing_reg: EnvRegistry, ptr: EnvironmentPointer) {
            // Make sure all the directories exist
            let (flox, tmp_dir) = flox_instance();
            let dot_flox_path = tmp_dir.path().join(".flox");
            std::fs::create_dir_all(&dot_flox_path).unwrap();
            let canonical_dot_flox_path = CanonicalPath::new(&dot_flox_path).unwrap();
            // Seed the existing registry
            let reg_contents = serde_json::to_string(&existing_reg).unwrap();
            let reg_path = env_registry_path(&flox);
            std::fs::write(&reg_path, reg_contents).unwrap();
            // Do the registration
            ensure_registered(&flox, &canonical_dot_flox_path, &ptr).unwrap();
            // Check the registration
            let new_reg = read_environment_registry(&reg_path).unwrap().unwrap();
            let expected_hash = path_hash(&canonical_dot_flox_path);
            let entry = new_reg.entry_for_hash(&expected_hash).unwrap();
            prop_assert_eq!(&entry.latest_env().as_ref().unwrap().pointer, &ptr);
        }

        #[test]
        fn registered_envs_remain_sorted(mut entry: RegistryEntry, new_envs in vec(any::<EnvironmentPointer>(), 0..=3)) {
            let mut envs_before = entry.envs.clone();
            envs_before.sort_by_cached_key(|e| e.created_at);
            prop_assert_eq!(&envs_before, &entry.envs);
            for env in new_envs {
                entry.register_env(&env).unwrap();
                let mut sorted_envs = entry.envs.clone();
                sorted_envs.sort_by_cached_key(|e| e.created_at);
                prop_assert_eq!(&entry.envs, &sorted_envs);
            }
        }

        #[test]
        fn entries_deregister_envs(mut entry: RegistryEntry) {
            prop_assume!(!entry.envs.is_empty());
            let latest_env = entry.envs.iter().last().unwrap().clone();
            let n_envs_before = entry.envs.len();
            let removed = entry.deregister_env(&latest_env.pointer).unwrap();
            let n_envs_after = entry.envs.len();
            prop_assert_eq!(n_envs_after + 1, n_envs_before);
            prop_assert_eq!(latest_env, removed);
        }

        #[test]
        fn registry_deregisters_envs(mut reg: EnvRegistry) {
            prop_assume!(!reg.entries.is_empty());
            prop_assume!(!reg.entries[0].envs.is_empty());
            let hash = reg.entries[0].path_hash.clone();
            let envs_to_deregister = reg.entries[0].envs.iter().cloned().rev().collect::<Vec<_>>();
            for env in envs_to_deregister.iter() {
                let deregistered = reg.deregister_env(&hash, &env.pointer).unwrap();
                prop_assert_eq!(&deregistered, env);
            }
            // Empty entries should be removed
            prop_assert!(reg.entry_for_hash(&hash).is_none());
        }
    }

    #[test]
    fn garbage_collect_envs() {
        let (flox, _temp_dir) = flox_instance();
        let reg_path = env_registry_path(&flox);

        // This also registers the environment.
        let env = new_path_environment(&flox, "version = 1");
        let env_hash = path_hash(&env.path);

        let reg_gc = garbage_collect(&flox).unwrap();
        let reg_read = read_environment_registry(&reg_path).unwrap().unwrap();
        assert_eq!(
            reg_gc, reg_read,
            "registry returned by GC should match what's on disk"
        );
        assert!(
            reg_read.entry_for_hash(&env_hash).is_some(),
            "should survive GC when it exists on disk, reg: {:#?}",
            reg_read
        );

        std::fs::remove_dir_all(&env.path).unwrap();

        let reg_gc = garbage_collect(&flox).unwrap();
        let reg_read = read_environment_registry(&reg_path).unwrap().unwrap();
        assert_eq!(
            reg_gc, reg_read,
            "registry returned by GC should match what's on disk"
        );
        assert!(
            reg_read.entry_for_hash(&env_hash).is_none(),
            "should not survive GC when deleted from disk, reg: {:#?}",
            reg_read
        );
    }
}
