use flox_rust_sdk::data::CanonicalizeError;
use flox_rust_sdk::models::environment::managed_environment::{
    GENERATION_LOCK_FILENAME,
    ManagedEnvironmentError,
};
use flox_rust_sdk::models::environment::remote_environment::RemoteEnvironmentError;
use flox_rust_sdk::models::environment::{
    CoreEnvironmentError,
    ENVIRONMENT_POINTER_FILENAME,
    EnvironmentError,
    UpgradeError,
};
use flox_rust_sdk::models::floxmeta::FloxMetaError;
use flox_rust_sdk::models::lockfile::ResolveError;
use flox_rust_sdk::providers::git::GitRemoteCommandError;
use flox_rust_sdk::providers::services::{LoggedError, ServiceError};
use indoc::{formatdoc, indoc};
use tracing::trace;

use crate::commands::EnvironmentSelectError;

pub fn format_error(err: &EnvironmentError) -> String {
    trace!("formatting environment_error: {err:?}");

    match err {
        EnvironmentError::DotFloxNotFound(_) => display_chain(err),

        // todo: enrich with a path?
        EnvironmentError::EnvDirNotFound => formatdoc! {"
            Found a '.flox' directory but unable to locate an environment directory.

            This is likely due to a corrupt environment.

            Try deleting the '.flox' directory and reinitializing the environment.
            If you cloned this environment from a remote repository, verify that
            `.flox/env/maifest.toml` is committed to the version control system.
        "},
        // todo: enrich with a path?
        EnvironmentError::EnvPointerNotFound => formatdoc! {"
            Found a '.flox' directory but unable to locate an 'env.json' in it.

            This is likely due to a corrupt environment.

            Try deleting the '.flox' directory and reinitializing the environment.
            If you cloned this environment from a remote repository, verify that
            `.flox/env.json` is committed to the version control system.
        "},

        // todo: enrich with a path?
        EnvironmentError::ManifestNotFound => formatdoc! {"
            Found a '.flox' directory but unable to locate a manifest file.

            This is likely due to a corrupt environment.

            Try deleting the '.flox' directory and reinitializing the environment.
            If you cloned this environment from a remote repository, verify that
            `.flox/env/maifest.toml` is committed to the version control system.
        "},

        // todo: enrich with a path?
        // see also the notes on [EnvironmentError2::InitEnv]
        EnvironmentError::InitEnv(err) => formatdoc! {"
            Failed to initialize environment.
            Could not prepare a '.flox' directory: {err}

            Please ensure that you have write permissions to the current directory.
        "},

        // todo: update when we implement `flox init --force`
        EnvironmentError::EnvironmentExists(path) => formatdoc! {"
            Found an existing environment at {path:?}.

            Please initialize a new environment in a different directory.

            If you are trying to reinitialize an existing environment,
            delete the existing environment using 'flox delete -d {path:?}' and try again.
        "},

        // These errors should rarely happen.
        // At this point, we already proved that we can write to the directory.
        EnvironmentError::WriteGitignore(_) => display_chain(err),
        EnvironmentError::WriteGitattributes(_) => display_chain(err),

        // todo: enrich with a path?
        EnvironmentError::ReadEnvironmentMetadata(error) => formatdoc! {"
            Failed to read environment metadata: {error}

            This is likely due to a corrupt environment.

            Try deleting the '.flox' directory and reinitializing the environment.
            If you cloned this environment from a remote repository, verify that
            `.flox/env.json` is committed to the version control system.
        "},
        // todo: enrich with a path?
        // todo: when can this happen:
        //   * user manually edited this
        //   * user pushed environment but did not commit the changes to env.json
        //   * new version of the file format and we don't support it yet
        //     or not anymore with the current version of flox
        //     (this should be caught earlier but you never know...)
        EnvironmentError::ParseEnvJson(error) => formatdoc! {"
            Failed to parse environment metadata: {error}

            This is likely due to a corrupt environment.

            Try deleting the '.flox' directory and reinitializing the environment.
            If you cloned this environment from a remote repository, verify that
            the latest changes to `.flox/env.json` are committed to the version control system.
        "},
        // this should always never be a problem and if it is, it's a bug
        // the user can likely not do anything about it
        // todo: add a note to user to report this as a bug?
        // todo: enrich with path
        EnvironmentError::SerializeEnvJson(_) => display_chain(err),
        EnvironmentError::WriteEnvJson(error) => formatdoc! {"
            Failed to write environment metadata: {error}

            Please ensure that you have write permissions to write '.flox/env.json'.
        "},

        // todo: where in the control flow does this happen?
        //       do we want a separate error type for this (likely)
        EnvironmentError::StartDiscoveryDir(CanonicalizeError { path, err }) => formatdoc! {"
            Failed to start discovery for flox environments in {path:?}: {err}

            Please ensure that the path exists and that you have read permissions.
        "},
        // unreachable when using the cli
        EnvironmentError::InvalidPath(_) => display_chain(err),

        // todo: where in the control flow does this happen?
        //       do we want a separate error type for this (likely)
        // Its also a somewhat weird to downcast to this error type
        // better to separate this into a separate error types.
        EnvironmentError::InvalidDotFlox { path, source } => {
            let source = if let Some(source) = source.downcast_ref::<EnvironmentError>() {
                format_error(source)
            } else {
                display_chain(&**source)
            };

            formatdoc! {"
                Found a '.flox' directory at {path:?},
                but it is not a valid flox environment:

                {source}
            "}
        },
        // todo: how to surface these internal errors?
        EnvironmentError::DiscoverGitDirectory(_) => formatdoc! {"
            Failed to discover git directory.

            See the run again with `--verbose` for more information.
        "},
        // todo: enrich with path
        EnvironmentError::DeleteEnvironment(err) => formatdoc! {"
            Failed to delete environment .flox directory: {err}

            Try manually deleting the '.flox' directory.
        "},
        // todo: enrich with path
        EnvironmentError::ReadManifest(err) => formatdoc! {"
            Failed to read manifest: {err}

            Please make sure that '.flox/env/manifest.toml' exists
            and that you have read permissions.
        "},

        // todo: enrich with path
        EnvironmentError::WriteManifest(err) => formatdoc! {"
            Failed to write manifest: {err}

            Please make sure that '.flox/env/manifest.toml' exists
            and that you have write permissions.
        "},

        // todo: enrich with path
        EnvironmentError::CreateGcRootDir(err) => format! {"
            Failed to create '.flox/run' directory: {err}

            Please make sure that you have write permissions to '.flox'.
        "},
        EnvironmentError::Core(core_error) => format_core_error(core_error),
        EnvironmentError::ManagedEnvironment(managed_error) => format_managed_error(managed_error),
        EnvironmentError::RemoteEnvironment(remote_error) => format_remote_error(remote_error),
        _ => display_chain(err),
    }
}

pub fn format_core_error(err: &CoreEnvironmentError) -> String {
    trace!("formatting core_error: {err:?}");

    match err {
        CoreEnvironmentError::ModifyToml(toml_error) => formatdoc! {"
            Failed to modify manifest.

            {toml_error}
        "},
        // todo: enrich with path
        // raised during edit
        CoreEnvironmentError::DeserializeManifest(err) => formatdoc! {
            "Failed to parse manifest:

            {err}
        ",
            // The message adds a newline at the end,
            // trim to make the error look better
            err = err.message().trim()
        },
        CoreEnvironmentError::MigrateManifest(err) => formatdoc! {
            "Could not automatically migrate manifest to version 1:

            {err}

            Use 'flox edit' to resolve errors and then try again.
        ",
            // The message adds a newline at the end,
            // trim to make the error look better
            err = err.message().trim()
        },
        CoreEnvironmentError::LockForMigration(err) => formatdoc! {
            "Failed to create version 1 lock:

            {err}

            Use 'flox edit' to resolve errors and then try again.
        ",
            err = format_core_error(err)
        },
        CoreEnvironmentError::MakeSandbox(_) => display_chain(err),
        // within transaction, user should not see this and likely can't do anything about it
        CoreEnvironmentError::WriteLockfile(_) => display_chain(err),
        CoreEnvironmentError::MakeTemporaryEnv(_) => display_chain(err),
        CoreEnvironmentError::PriorTransaction(backup) => {
            let mut env_path = backup.clone();
            env_path.set_file_name("env");
            formatdoc! {"
                Found a transaction backup at {backup:?}.

                This indicates that a previous transaction was interrupted.

                Please restore the backup by moving {backup:?} -> {env_path:?}
                or delete the {backup:?} directory.
            "}
        },
        CoreEnvironmentError::BackupTransaction(err) => formatdoc! {"
            Failed to backup current environment directory: {err}

            Please ensure that you have write permissions to '.flox/*'."
        },
        CoreEnvironmentError::AbortTransaction(err) => formatdoc! {"
            Failed to abort transaction: {err}

            Please ensure that you have write permissions to '.flox/*'."
        },
        CoreEnvironmentError::Move(err) => formatdoc! {"
            Failed to commit transaction: {err}

            Could not move modified environment directory to original location.
        "},
        CoreEnvironmentError::RemoveBackup(err) => formatdoc! {"
            Failed to remove transaction backup: {err}

            Please ensure that you have write permissions to '.flox/*'.
        "},

        // these are out of our user's control as these errors are within the transaction
        // todo: adapt wordnig?
        // todo: enrich with path
        CoreEnvironmentError::OpenManifest(err) => formatdoc! {"
            Failed to open manifest for reading: {err}

            Please ensure that you have read permissions to '.flox/env/manifest.toml'.
        "},
        // todo: enrich with path
        CoreEnvironmentError::UpdateManifest(err) => formatdoc! {"
            Failed to write to manifest file: {err}

            Please ensure that you have write permissions to '.flox/env/manifest.toml'.
        "},

        // internal error, a bug if this happens to users!
        CoreEnvironmentError::BadLockfilePath(_) => display_chain(err),

        CoreEnvironmentError::BuildEnv(err) => formatdoc! {"
            Failed to build environment:

            {err}
        ", err = display_chain(err)},

        CoreEnvironmentError::Resolve(locked_manifest_error) => {
            format_resolve_error(locked_manifest_error)
        },

        CoreEnvironmentError::UpgradeFailedCatalog(err) => match err {
            UpgradeError::PkgNotFound(err) => err.to_string(),
            UpgradeError::NonEmptyNamedGroup { pkg, group } => formatdoc! {"
                '{pkg}' is a package in the group '{group}' with multiple packages.
                To upgrade the group, specify the group name:
                    $ flox upgrade {group}
                To upgrade all packages, run:
                    $ flox upgrade
            "},
        },
        CoreEnvironmentError::UninstallError(_) => display_chain(err),
        // User facing
        CoreEnvironmentError::Services(err) => display_chain(err),

        // this is a bug, but likely needs some formatting
        CoreEnvironmentError::ReadLockfile(_) => display_chain(err),
        CoreEnvironmentError::ParseLockfile(serde_error) => formatdoc! {"
            Failed to parse lockfile as JSON: {serde_error}

            This is likely due to a corrupt environment.
        "},
        CoreEnvironmentError::CreateTempdir(_) => display_chain(err),
        CoreEnvironmentError::Auth(err) => display_chain(err),
    }
}

pub fn format_managed_error(err: &ManagedEnvironmentError) -> String {
    trace!("formatting managed_environment_error: {err:?}");

    match err {
        // todo: communicate reasons for this error
        // git auth errors may be caught separately or reported
        ManagedEnvironmentError::OpenFloxmeta(err)
        | ManagedEnvironmentError::UpdateFloxmeta(err) => formatdoc! {"
            Failed to fetch environment: {err}
        "},

        // todo: merge errors or make more specific
        // now they represent the same thing.
        ManagedEnvironmentError::Fetch(err) | ManagedEnvironmentError::FetchUpdates(err) => {
            formatdoc! {"
            Failed to fetch updates for environment: {err}

            Please ensure that you have network connectivity
            and access to the remote environment.
        "}
        },
        ManagedEnvironmentError::CheckGitRevision(_) => display_chain(err),
        ManagedEnvironmentError::CheckBranchExists(_) => display_chain(err),
        ManagedEnvironmentError::LocalRevDoesNotExist => formatdoc! {"
            The environment lockfile refers to a version of the environment
            that does not exist locally.

            This can happen if the environment is modified on another machine,
            and the lockfile is committed to the version control system
            before the environment is pushed.

            To resolve this issue, either
             * remove '.flox/{GENERATION_LOCK_FILENAME}' (this will reset the environment to the latest version)
             * push the environment on the remote machine and commit the updated lockfile
        "},
        ManagedEnvironmentError::RevDoesNotExist => formatdoc! {"
            The environment lockfile refers to a version of the environment
            that does not exist locally or on the remote.

            This can happen if the environment was force-pushed
            after the lockfile was committed to the version control system.

            To resolve this issue, remove '.flox/{GENERATION_LOCK_FILENAME}' (this will reset the environment to the latest version)
        "},
        ManagedEnvironmentError::InvalidLock(err) => formatdoc! {"
            The environment lockfile is invalid: {err}

            This can happen if the lockfile was manually edited.

            To resolve this issue, remove '.flox/{GENERATION_LOCK_FILENAME}' (this will reset the environment to the latest version)
        "},
        ManagedEnvironmentError::ReadPointerLock(err) => formatdoc! {"
            Failed to read pointer lockfile: {err}

            Please ensure that you have read permissions to '.flox/{GENERATION_LOCK_FILENAME}'.
        "},
        // various internal git errors while acting on the floxmeta repo
        ManagedEnvironmentError::Git(_) => display_chain(err),
        ManagedEnvironmentError::GitBranchHash(_) => display_chain(err),
        ManagedEnvironmentError::WriteLock(err) => formatdoc! {"
            Failed to write to lockfile: {err}

            Please ensure that you have write permissions to '.flox/{GENERATION_LOCK_FILENAME}'
        "},
        ManagedEnvironmentError::SerializeLock(_) => display_chain(err),

        // the following two errors are related to create reverse links to the .flox directory
        // those are internal errors but may arise if the user does not have write permissions to
        // xdg_data_home
        // todo: expose as rich error or unexpected error?
        ManagedEnvironmentError::ReverseLink(_) => display_chain(err),
        ManagedEnvironmentError::CreateLinksDir(_) => display_chain(err),

        ManagedEnvironmentError::CreateLocalEnvironmentView(err) => formatdoc! {"
            Failed to create the local environment from the current generation: {err}

            Please ensure that you have read and write permissions
            to the environment directory in '.flox/env'.
        "},

        ManagedEnvironmentError::CheckoutOutOfSync => indoc! {"
            Your environment has changes that are not yet synced to a generation.

            To resolve this issue, run either
            * 'flox edit --sync' to commit your local changes to a new generation
            * 'flox edit --reset' to discard your local changes and reset to the latest generation
        "}
        .to_string(),

        ManagedEnvironmentError::ReadLocalManifest(_) => display_chain(err),
        ManagedEnvironmentError::Generations(_) => display_chain(err),

        ManagedEnvironmentError::BadBranchName(_) => display_chain(err),

        // currently unused
        ManagedEnvironmentError::ProjectNotFound { .. } => display_chain(err),

        // todo: enrich with url
        ManagedEnvironmentError::InvalidFloxhubBaseUrl(err) => formatdoc! {"
            The FloxHub base url set in the config is invalid: {err}

            Please ensure that the url
            * is either a valid http or https url
            * has a valid domain name
            * is not an IP address or 'localhost'
        "},

        ManagedEnvironmentError::Diverged => formatdoc! {"
            The environment has diverged from the remote.

            This can happen if the environment is modified and pushed from another machine.

            To resolve this issue, either
             * run 'flox pull --force'
               to discard local changes
               and reset the environment to the latest upstream version.
             * run 'flox push --force'
               to overwrite the remote environment with the local changes.
               Attention: this will discard any changes made on the remote machine
               and cause conflicts when the remote machine tries to pull or push!
        "},
        ManagedEnvironmentError::AccessDenied => formatdoc! {"
            Access denied to the remote environment.

            This can happen if the remote is not owned by you
            or the owner did not grant you access.

            Please check the spelling of the remote environment
            and make sure that you have access to it.
        "},
        ManagedEnvironmentError::UpstreamNotFound {
            env_ref,
            upstream: _,
            user,
        } => {
            let by_current_user = user
                .as_ref()
                .map(|u| u == env_ref.owner().as_str())
                .unwrap_or_default();
            let message = "Environment not found in FloxHub.";
            if by_current_user {
                formatdoc! {"
                    {message}

                    You can run 'flox push' to push the environment back to FloxHub.
                "}
            } else {
                message.to_string()
            }
        },
        // access denied is caught early as ManagedEnvironmentError::AccessDenied
        ManagedEnvironmentError::Push(_) => display_chain(err),
        ManagedEnvironmentError::PushWithLocalIncludes => display_chain(err),
        ManagedEnvironmentError::DeleteBranch(_) => display_chain(err),
        ManagedEnvironmentError::DeleteEnvironment(path, err) => formatdoc! {"
            Failed to delete remote environment at {path:?}: {err}

            Please ensure that you have write permissions to {path:?}.
        "},
        ManagedEnvironmentError::DeleteEnvironmentLink(_, _) => display_chain(err),
        ManagedEnvironmentError::DeleteEnvironmentReverseLink(_, _) => display_chain(err),
        ManagedEnvironmentError::ApplyUpdates(_) => display_chain(err),
        // todo: unwrap this error to report more precisely?
        //       this should this is a bug if this happens to users.
        ManagedEnvironmentError::InitializeFloxmeta(_) => display_chain(err),
        ManagedEnvironmentError::SerializePointer(_) => display_chain(err),
        ManagedEnvironmentError::WritePointer(err) => formatdoc! {"
            Failed to write to pointer: {err}

            Please ensure that you have write permissions to '.flox/{ENVIRONMENT_POINTER_FILENAME}'.
        "},
        ManagedEnvironmentError::CreateFloxmetaDir(_) => display_chain(err),
        ManagedEnvironmentError::CreateGenerationFiles(_) => display_chain(err),
        ManagedEnvironmentError::CommitGeneration(err) => formatdoc! {"
            Failed to create a new generation: {err}

            This may be due to a corrupt environment
            or another process modifying the environment.

            Please try again later.
        "},

        ManagedEnvironmentError::ReadManifest(e) => formatdoc! {"
            Could not read managed manifest.

            {err}
        ",err = display_chain(e) },
        ManagedEnvironmentError::CanonicalizePath(canonicalize_err) => formatdoc! {"
            Invalid path to environment: {canonicalize_err}

            Please ensure that the path exists and that you have read permissions.
        "},
        ManagedEnvironmentError::Build(core_environment_error) => {
            format_core_error(core_environment_error)
        },
        ManagedEnvironmentError::Link(core_environment_error) => {
            format_core_error(core_environment_error)
        },

        ManagedEnvironmentError::Registry(_) => display_chain(err),

        ManagedEnvironmentError::Core(core_environment_error) => {
            format_core_error(core_environment_error)
        },
    }
}

pub fn format_remote_error(err: &RemoteEnvironmentError) -> String {
    trace!("formatting remote_environment_error: {err:?}");

    match err {
        RemoteEnvironmentError::OpenManagedEnvironment(err) => formatdoc! {"
            Failed to open cloned remote environment: {err}

            This may be due to a corrupt or incompatible environment.
        ", err = display_chain(err)},

        RemoteEnvironmentError::CreateTempDotFlox(_) => formatdoc! {"
            Failed to initialize remote environment locally.

            Please ensure that you have write permissions to FLOX_CACHE_DIR/remote.
        "},

        RemoteEnvironmentError::ResetManagedEnvironment(ManagedEnvironmentError::FetchUpdates(
            GitRemoteCommandError::RefNotFound(_),
        ))
        | RemoteEnvironmentError::GetLatestVersion(FloxMetaError::CloneBranch(
            GitRemoteCommandError::AccessDenied,
        ))
        | RemoteEnvironmentError::GetLatestVersion(FloxMetaError::CloneBranch(
            GitRemoteCommandError::RefNotFound(_),
        )) => formatdoc! {"
            Environment not found in FloxHub.
            "},

        RemoteEnvironmentError::ResetManagedEnvironment(err) => formatdoc! {"
            Failed to reset remote environment to latest upstream version:

            {err}
            ", err = format_managed_error(err)},
        RemoteEnvironmentError::GetLatestVersion(err) => formatdoc! {"
            Failed to get latest version of remote environment: {err}

            ", err = display_chain(err)},
        RemoteEnvironmentError::UpdateUpstream(ManagedEnvironmentError::Diverged) => formatdoc! {"
            The remote environment has diverged.

            This can happen if the environment is modified and pushed from another machine
            at the same time.

            Please try again after verifying the concurrent changes.
        "},
        RemoteEnvironmentError::UpdateUpstream(ManagedEnvironmentError::AccessDenied) => {
            formatdoc! {"
            Access denied to the remote environment.

            This can happen if the remote is not owned by you
            or the owner did not grant you access.

            Please check the spelling of the remote environment
            and make sure that you have access to it.
        "}
        },
        RemoteEnvironmentError::UpdateUpstream(_) => display_chain(err),
        RemoteEnvironmentError::InvalidTempPath(_) => display_chain(err),
        RemoteEnvironmentError::ReadInternalOutLink(_) => display_chain(err),
        RemoteEnvironmentError::DeleteOldOutLink(_) => display_chain(err),
        RemoteEnvironmentError::WriteNewOutlink(_) => display_chain(err),
        RemoteEnvironmentError::CreateGcRootDir(_) => display_chain(err),
    }
}

pub fn format_environment_select_error(err: &EnvironmentSelectError) -> String {
    trace!("formatting environment_select_error: {err:?}");

    match err {
        EnvironmentSelectError::EnvironmentError(err) => format_error(err),
        EnvironmentSelectError::EnvNotFoundInCurrentDirectory => formatdoc! {"
            Did not find an environment in the current directory.
        "},
        EnvironmentSelectError::Anyhow(err) => err
            .chain()
            .skip(1)
            .fold(err.to_string(), |acc, cause| format!("{acc}: {cause}")),
    }
}

pub fn format_resolve_error(err: &ResolveError) -> String {
    trace!("formatting locked_manifest_error: {err:?}");
    match err {
        // region: errors from the catalog locking
        ResolveError::CatalogResolve(err) => display_chain(err),
        // endregion
        ResolveError::UnrecognizedSystem(system) => formatdoc! {"
            Unrecognized system in manifest: {system}

            Supported systems are: aarch64-linux, x86_64-linux, aarch64-darwin, x86_64-darwin
        "},

        ResolveError::SystemUnavailableInManifest { .. } => display_chain(err),

        ResolveError::ResolutionFailed(_) => display_chain(err),
        // User facing
        ResolveError::LicenseNotAllowed(..) => display_chain(err),
        // User facing
        ResolveError::BrokenNotAllowed(_) => display_chain(err),
        // User facing
        ResolveError::UnfreeNotAllowed(_) => display_chain(err),
        ResolveError::MissingPackageDescriptor(_) => display_chain(err),
        ResolveError::LockFlakeNixError(_) => display_chain(err),
        ResolveError::InstallIdNotInManifest(_) => display_chain(err),
    }
}

pub fn format_service_error(err: &ServiceError) -> String {
    match err {
        ServiceError::LoggedError(LoggedError::ServiceManagerUnresponsive(socket)) => formatdoc! {"
            The service manager is unresponsive, please retry later.

            If the problem persists, delete {socket}
            and restart services with 'flox activate --start-services'
            or 'flox services start' from an existing activation.
        ", socket = socket.display()},
        ServiceError::LoggedError(LoggedError::SocketDoesntExist) => formatdoc! {"
            Services not started or quit unexpectedly.

            To start services, run 'flox services start' in an activated environment,
            or activate the environment with 'flox activate --start-services'.
        "},
        _ => display_chain(err),
    }
}

/// Displays and formats a chain of errors connected via their `source` attribute.
pub fn display_chain(mut err: &dyn std::error::Error) -> String {
    let mut fmt = err.to_string();
    while let Some(source) = err.source() {
        fmt = format!("{fmt}: {source}");
        err = source;
    }

    fmt
}
