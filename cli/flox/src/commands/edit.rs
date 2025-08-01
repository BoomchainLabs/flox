use std::env;
use std::fs::File;
use std::io::stdin;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use bpaf::Bpaf;
use flox_rust_sdk::flox::{EnvironmentName, Flox};
use flox_rust_sdk::models::environment::managed_environment::{
    ManagedEnvironmentError,
    SyncToGenerationResult,
};
use flox_rust_sdk::models::environment::{
    ConcreteEnvironment,
    CoreEnvironmentError,
    EditResult,
    Environment,
    EnvironmentError,
};
use flox_rust_sdk::providers::buildenv::BuildEnvError;
use flox_rust_sdk::providers::services::ServiceError;
use itertools::Itertools;
use tracing::{debug, instrument};

use super::services::warn_manifest_changes_for_services;
use super::{
    EnvironmentSelect,
    UninitializedEnvironment,
    activated_environments,
    environment_select,
};
use crate::commands::{EnvironmentSelectError, ensure_floxhub_token};
use crate::utils::dialog::{Confirm, Dialog};
use crate::utils::errors::format_error;
use crate::utils::message;
use crate::{environment_subcommand_metric, subcommand_metric};

// Edit declarative environment configuration
#[derive(Bpaf, Clone)]
pub struct Edit {
    #[bpaf(external(environment_select), fallback(Default::default()))]
    environment: EnvironmentSelect,

    #[bpaf(external(edit_action), fallback(EditAction::EditManifest{file: None}))]
    action: EditAction,
}
#[derive(Bpaf, Clone)]
pub enum EditAction {
    EditManifest {
        /// Replace environment manifest with that in <file>
        #[bpaf(long, short, argument("file"))]
        file: Option<PathBuf>,
    },

    Rename {
        /// Rename the environment to <name>
        #[bpaf(long, short, argument("name"))]
        name: EnvironmentName,
    },

    /// Commit local environment changes to a new generation
    ///
    /// (Only available for managed environments)
    #[bpaf(long, short)]
    Sync,

    /// Discard local changes and reset to the latest generation
    ///
    /// (Only available for managed environments)
    #[bpaf(long)]
    Reset,
}

impl Edit {
    #[instrument(name = "edit", skip_all)]
    pub async fn handle(self, mut flox: Flox) -> Result<()> {
        // Record subcommand metric prior to environment_subcommand_metric below
        // in case we error before then
        subcommand_metric!("edit");

        // Ensure the user is logged in for the following remote operations
        if let EnvironmentSelect::Remote(_) = self.environment {
            ensure_floxhub_token(&mut flox).await?;
        };

        let mut detected_environment =
            match self.environment.detect_concrete_environment(&flox, "Edit") {
                Ok(concrete_env) => concrete_env,
                Err(EnvironmentSelectError::Anyhow(e)) => Err(e)?,
                Err(e) => Err(e)?,
            };
        environment_subcommand_metric!("edit", detected_environment);

        match self.action {
            EditAction::EditManifest { file } => {
                // TODO: differentiate between interactive edits and replacement
                let span = tracing::info_span!("edit_file");
                let _guard = span.enter();

                let contents = Self::provided_manifest_contents(file)?;

                Self::edit_manifest(&flox, &mut detected_environment, contents).await?
            },
            EditAction::Rename { name } => {
                let span = tracing::info_span!("rename");
                let _guard = span.enter();
                if let ConcreteEnvironment::Path(ref mut environment) = detected_environment {
                    let old_name = environment.name();
                    if name == old_name {
                        bail!("environment already named '{name}'");
                    }
                    environment.rename(name.clone())?;
                    message::updated(format!("renamed environment '{old_name}' to '{name}'"));
                } else {
                    // todo: handle remote environments in the future
                    bail!("Cannot rename environments on FloxHub");
                }
            },

            EditAction::Sync => {
                let span = tracing::info_span!(
                    "sync",
                    progress = "Syncing environment to a new generation"
                );
                let _guard = span.enter();
                let ConcreteEnvironment::Managed(ref mut environment) = detected_environment else {
                    bail!("Cannot sync local or remote environments.");
                };

                let sync_result = environment.create_generation_from_local_env(&flox)?;
                match sync_result {
                    SyncToGenerationResult::UpToDate => message::plain("No local changes to sync."),
                    SyncToGenerationResult::Synced => {
                        message::updated("Environment successfully synced to a new generation.")
                    },
                }
            },

            EditAction::Reset => {
                let span = tracing::info_span!(
                    "reset",
                    progress = "Resetting environment to current generation"
                );
                let _guard = span.enter();
                let ConcreteEnvironment::Managed(ref mut environment) = detected_environment else {
                    bail!("Cannot reset local or remote environments.");
                };

                environment.reset_local_env_to_current_generation(&flox)?;

                // The current generation already has a lock,
                // so we can skip locking.
                let store_path = environment.build(&flox)?;
                environment.link(&store_path)?;

                message::updated("Environment changes reset to current generation.");
            },
        };

        Ok(())
    }

    async fn edit_manifest(
        flox: &Flox,
        environment: &mut ConcreteEnvironment,
        contents: Option<String>,
    ) -> Result<()> {
        if let ConcreteEnvironment::Managed(environment) = environment {
            if environment.has_local_changes(flox)? && contents.is_none() {
                bail!(ManagedEnvironmentError::CheckoutOutOfSync)
            }
        };

        let active_environment = UninitializedEnvironment::from_concrete_environment(environment);

        let result = match contents {
            // If provided with the contents of a manifest file, either via a path to a file or via
            // contents piped to stdin, use those contents to try building the environment.
            Some(new_manifest) => environment.edit(flox, new_manifest)?,
            // If not provided with new manifest contents, let the user edit the file directly
            // via $EDITOR or $VISUAL (as long as `flox edit` was invoked interactively).
            None => Self::interactive_edit(flox, environment).await?,
        };

        // outside the match to avoid rustfmt falling on its face
        let reactivate_required_note = indoc::indoc! {"
            Your manifest has changes that cannot be automatically applied.

            Please 'exit' the shell and run 'flox activate' to see these changes.
       "};

        match result {
            EditResult::Unchanged => {
                message::warning("No changes made to environment.");
            },
            EditResult::Changed {
                ref old_lockfile,
                ref new_lockfile,
                ..
            } => {
                if result.reactivate_required()
                    && activated_environments().is_active(&active_environment)
                {
                    message::warning(reactivate_required_note);
                } else {
                    message::updated("Environment successfully updated.")
                }

                warn_manifest_changes_for_services(flox, environment);

                if new_lockfile.compose.is_some() {
                    message::print_overridden_manifest_fields(new_lockfile);
                    message::info("Run 'flox list -c' to see merged manifest.");
                }

                // breadcrumb metric to estimate use of composition
                let old_includes = old_lockfile
                    .as_ref()
                    .as_ref()
                    .and_then(|lf| lf.compose.as_ref())
                    .map(|compose| &compose.include);
                let new_includes = new_lockfile
                    .compose
                    .as_ref()
                    .map(|compose| &compose.include);
                let edited_includes = old_includes != new_includes;
                subcommand_metric!("edit", "edited_includes" = edited_includes);
            },
        }

        Ok(())
    }

    /// Interactively edit the manifest file
    async fn interactive_edit(
        flox: &Flox,
        environment: &mut dyn Environment,
    ) -> Result<EditResult> {
        if !Dialog::can_prompt() {
            bail!("Can't edit interactively in non-interactive context")
        }

        let (editor, args) = Self::determine_editor()?;

        // Make a copy of the manifest for the user to edit so failed edits aren't left in
        // the original manifest. You can't put creation/cleanup inside the `edited_manifest_contents`
        // method because the temporary manifest needs to stick around in case the user wants
        // or needs to make successive edits without starting over each time.
        let tmp_manifest = tempfile::Builder::new()
            .prefix("manifest.")
            .suffix(".toml")
            .tempfile_in(&flox.temp_dir)?;
        std::fs::write(&tmp_manifest, environment.manifest_contents(flox)?)?;

        let should_continue_dialog = Dialog {
            message: "Continue editing?",
            help_message: Default::default(),
            typed: Confirm {
                default: Some(true),
            },
        };

        // Let the user keep editing the file until the build succeeds or the user
        // decides to stop.
        loop {
            let new_manifest = Edit::edited_manifest_contents(&tmp_manifest, &editor, &args)?;
            let result = environment.edit(flox, new_manifest.clone());
            match Self::make_interactively_recoverable(result)? {
                Ok(result) => return Ok(result),

                // for recoverable errors, prompt the user to continue editing
                Err(e) => {
                    message::error(format_error(&e));

                    if !Dialog::can_prompt() {
                        bail!("Can't prompt to continue editing in non-interactive context");
                    }
                    if !should_continue_dialog.clone().prompt().await? {
                        bail!("Environment editing cancelled");
                    }
                },
            }
        }
    }

    /// Returns `Ok` if the edit result is successful or recoverable, `Err` otherwise
    fn make_interactively_recoverable(
        result: Result<EditResult, EnvironmentError>,
    ) -> Result<Result<EditResult, EnvironmentError>, EnvironmentError> {
        match result {
            Err(e @ EnvironmentError::Core(CoreEnvironmentError::Resolve(_)))
            | Err(e @ EnvironmentError::Core(CoreEnvironmentError::DeserializeManifest(_)))
            | Err(
                e @ EnvironmentError::Core(CoreEnvironmentError::BuildEnv(
                    BuildEnvError::Realise2 { .. } | BuildEnvError::Build(_),
                )),
            )
            | Err(
                e @ EnvironmentError::Core(CoreEnvironmentError::Services(
                    ServiceError::InvalidConfig(_),
                )),
            )
            | Err(e @ EnvironmentError::Recoverable(_)) => Ok(Err(e)),
            Err(e) => Err(e),
            Ok(result) => Ok(Ok(result)),
        }
    }

    /// Determines the editor to use for interactive editing, based on the environment
    /// Returns the editor and a list of args to pass to the editor
    ///
    /// If $VISUAL or $EDITOR is set, use that.
    /// The editor cannot be an empty string or one that consists of fully Unicode whitespace.
    /// Arguments can be passed and will be split on whitespace.
    /// Otherwise, try to find a known editor in $PATH.
    /// The known editor selected is the first one found in $PATH from the following list:
    ///
    ///   vim, vi, nano, emacs.
    fn determine_editor() -> Result<(PathBuf, Vec<String>)> {
        Self::determine_editor_from_vars(
            env::var("VISUAL").unwrap_or_default(),
            env::var("EDITOR").unwrap_or_default(),
            env::var("PATH").context("$PATH not set")?,
        )
    }

    /// Determines the editor to use for interactive editing, based on passed values
    /// Returns the editor and a list of args to pass to the editor
    fn determine_editor_from_vars(
        visual_var: String,
        editor_var: String,
        path_var: String,
    ) -> Result<(PathBuf, Vec<String>)> {
        let var = if !visual_var.trim().is_empty() {
            visual_var
        } else {
            editor_var
        };
        let mut command = var.split_whitespace();

        let editor = command.next().unwrap_or_default().to_owned();
        let args = command.map(|s| s.to_owned()).collect();

        if !editor.is_empty() {
            debug!("Using configured editor {:?} with args {:?}", editor, args);
            return Ok((PathBuf::from(editor), args));
        }

        let path_entries = env::split_paths(&path_var).collect::<Vec<_>>();

        let (editor, path) = ["nano", "vim", "vi", "emacs"]
            .iter()
            .cartesian_product(path_entries)
            .find(|(editor, path)| path.join(editor).is_file())
            .context("no known editor found in $PATH")?;

        debug!("Using default editor {:?} from {:?}", editor, path);

        Ok((path.join(editor), vec![]))
    }

    /// Retrieves the new manifest file contents if a new manifest file was provided
    fn provided_manifest_contents(file: Option<PathBuf>) -> Result<Option<String>> {
        if let Some(ref file) = file {
            let mut file: Box<dyn std::io::Read + Send> = if file == Path::new("-") {
                Box::new(stdin())
            } else {
                Box::new(File::open(file)?)
            };

            let mut contents = String::new();
            file.read_to_string(&mut contents)?;
            Ok(Some(contents))
        } else {
            Ok(None)
        }
    }

    /// Gets a new set of manifest contents after a user edits the file
    fn edited_manifest_contents(
        path: impl AsRef<Path>,
        editor: impl AsRef<Path>,
        args: impl AsRef<Vec<String>>,
    ) -> Result<String> {
        let mut command = Command::new(editor.as_ref());
        if !args.as_ref().is_empty() {
            command.args(args.as_ref());
        }
        command.arg(path.as_ref());

        let child = command.spawn().context("editor command failed")?;
        let _ = child.wait_with_output().context("editor command failed")?;

        let contents = std::fs::read_to_string(path)?;
        Ok(contents)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use flox_rust_sdk::flox::test_helpers::{flox_instance, flox_instance_with_optional_floxhub};
    use flox_rust_sdk::models::environment::managed_environment::test_helpers::mock_managed_environment_unlocked;
    use flox_rust_sdk::models::environment::path_environment::test_helpers::{
        new_path_environment,
        new_path_environment_in,
    };
    use flox_rust_sdk::models::lockfile::{ResolutionFailures, ResolveError};
    use flox_rust_sdk::utils::logging::test_helpers::test_subscriber_message_only;
    use indoc::{formatdoc, indoc};
    use pretty_assertions::assert_eq;
    use serde::de::Error;
    use tempfile::tempdir;
    use tracing::instrument::WithSubscriber;

    use super::*;

    /// successful edit returns value that will end the loop
    #[test]
    fn test_recover_edit_loop_result_success() {
        let result = Ok(EditResult::Unchanged);

        Edit::make_interactively_recoverable(result)
            .expect("should return Ok")
            .expect("should return Ok");
    }

    /// errors parsing the manifest are recoverable
    #[test]
    fn test_recover_edit_loop_result_bad_manifest() {
        let result = Err(EnvironmentError::Core(
            CoreEnvironmentError::DeserializeManifest(toml::de::Error::custom("msg")),
        ));

        Edit::make_interactively_recoverable(result)
            .expect("should be recoverable")
            .expect_err("should return recoverable Err");
    }

    /// errors locking the manifest are recoverable
    #[test]
    fn test_recover_edit_loop_result_locking() {
        let result = Err(EnvironmentError::Core(CoreEnvironmentError::Resolve(
            ResolveError::ResolutionFailed(ResolutionFailures(vec![])),
        )));

        Edit::make_interactively_recoverable(result)
            .expect("should be recoverable")
            .expect_err("should return recoverable err");
    }

    /// Error due to empty vars and no editor in PATH
    #[test]
    fn test_determine_editor_from_vars_not_found() {
        let visual_var = "".to_owned();
        let editor_var = "".to_owned();

        let tmp1 = tempdir().expect("should create tempdir");
        let tmp2 = tempdir().expect("should create tempdir");
        let tmp3 = tempdir().expect("should create tempdir");

        let path_var = std::env::join_paths([&tmp1, &tmp2, &tmp3].map(|d| d.path()))
            .expect("should path-join tmpdirs")
            .into_string()
            .expect("should convert paths from OsString to String");

        Edit::determine_editor_from_vars(visual_var, editor_var, path_var)
            .expect_err("should error with editor not found");

        // ensure tempdir lifetimes do not drop -- require tempdir to exist on fs through the end of the test
        assert!(tmp1.path().is_dir());
        assert!(tmp2.path().is_dir());
        assert!(tmp3.path().is_dir());
    }

    /// Default to the first of any editor while traversing PATH
    #[test]
    fn test_determine_editor_from_vars_first_default_editor() {
        let visual_var = "".to_owned();
        let editor_var = "".to_owned();

        let tmp1 = tempdir().expect("should create tempdir");
        let tmp2 = tempdir().expect("should create tempdir");
        let tmp3 = tempdir().expect("should create tempdir");

        let path_var = std::env::join_paths([&tmp1, &tmp2, &tmp3].map(|d| d.path().to_owned()))
            .expect("should path-join tmpdirs")
            .into_string()
            .expect("should convert paths from OsString to String");

        let nano = tmp1.path().join("nano");
        let vim = tmp2.path().join("vim");
        let vi = tmp2.path().join("vi");
        let emacs = tmp3.path().join("emacs");
        File::create(&nano).expect("should create file");
        File::create(vim).expect("should create file");
        File::create(vi).expect("should create file");
        File::create(emacs).expect("should create file");

        assert_eq!(
            Edit::determine_editor_from_vars(visual_var, editor_var, path_var)
                .expect("should determine default editor"),
            (nano, Vec::<String>::new())
        );

        // ensure tempdir lifetimes do not drop -- require tempdir to exist on fs through the end of the test
        assert!(tmp1.path().is_dir());
        assert!(tmp2.path().is_dir());
        assert!(tmp3.path().is_dir());
    }

    /// Do not default to directories
    #[test]
    fn test_determine_editor_from_vars_no_directory() {
        let visual_var = "".to_owned();
        let editor_var = "".to_owned();

        let tmp1 = tempdir().expect("should create tempdir");
        let tmp2 = tempdir().expect("should create tempdir");
        let tmp3 = tempdir().expect("should create tempdir");

        let path_var = std::env::join_paths([&tmp1, &tmp2, &tmp3].map(|d| d.path().to_owned()))
            .expect("should path-join tmpdirs")
            .into_string()
            .expect("should convert paths from OsString to String");

        let nano = tmp1.path().join("nano");
        let vim = tmp2.path().join("vim");
        let vi = tmp2.path().join("vi");
        let emacs = tmp3.path().join("emacs");

        fs::create_dir(nano).expect("should create directory");

        File::create(&vim).expect("should create file");
        File::create(vi).expect("should create file");
        File::create(emacs).expect("should create file");

        assert_eq!(
            Edit::determine_editor_from_vars(visual_var, editor_var, path_var)
                .expect("should determine default editor"),
            (vim, Vec::<String>::new())
        );

        // ensure tempdir lifetimes do not drop -- require tempdir to exist on fs through the end of the test
        assert!(tmp1.path().is_dir());
        assert!(tmp2.path().is_dir());
        assert!(tmp3.path().is_dir());
    }

    /// Return VISUAL before EDITOR, do not default to PATH
    #[test]
    fn test_determine_editor_from_vars_visual() {
        let visual_var = "micro".to_owned();
        let editor_var = "hx".to_owned();

        let tmp1 = tempdir().expect("should create tempdir");
        let tmp2 = tempdir().expect("should create tempdir");
        let tmp3 = tempdir().expect("should create tempdir");

        let path_var = std::env::join_paths([&tmp1, &tmp2, &tmp3].map(|d| d.path().to_owned()))
            .expect("should path-join tmpdirs")
            .into_string()
            .expect("should convert paths from OsString to String");

        let nano = tmp1.path().join("nano");
        let vim = tmp2.path().join("vim");
        let vi = tmp2.path().join("vi");
        let emacs = tmp3.path().join("emacs");
        File::create(nano).expect("should create file");
        File::create(vim).expect("should create file");
        File::create(vi).expect("should create file");
        File::create(emacs).expect("should create file");

        assert_eq!(
            Edit::determine_editor_from_vars(visual_var, editor_var, path_var)
                .expect("should determine default editor"),
            (PathBuf::from("micro"), Vec::<String>::new())
        );

        // ensure tempdir lifetimes do not drop -- require tempdir to exist on fs through the end of the test
        assert!(tmp1.path().is_dir());
        assert!(tmp2.path().is_dir());
        assert!(tmp3.path().is_dir());
    }

    /// Fallback to EDITOR, no default editor available in PATH
    #[test]
    fn test_determine_editor_from_vars_editor() {
        let visual_var = "".to_owned();
        let editor_var = "hx".to_owned();

        let tmp1 = tempdir().expect("should create tempdir");
        let tmp2 = tempdir().expect("should create tempdir");
        let tmp3 = tempdir().expect("should create tempdir");

        let path_var = std::env::join_paths([&tmp1, &tmp2, &tmp3].map(|d| d.path().to_owned()))
            .expect("should path-join tmpdirs")
            .into_string()
            .expect("should convert paths from OsString to String");

        assert_eq!(
            Edit::determine_editor_from_vars(visual_var, editor_var, path_var)
                .expect("should determine default editor"),
            (PathBuf::from("hx"), Vec::<String>::new())
        );

        // ensure tempdir lifetimes do not drop -- require tempdir to exist on fs through the end of the test
        assert!(tmp1.path().is_dir());
        assert!(tmp2.path().is_dir());
        assert!(tmp3.path().is_dir());
    }

    /// Split VISUAL into editor and args
    #[test]
    fn test_determine_editor_from_vars_visual_with_args() {
        let visual_var = "  code -w --reuse-window   --userdata-dir /home/user/code  ".to_owned();
        let editor_var = "hx".to_owned();

        let path_var = "".to_owned();

        assert_eq!(
            Edit::determine_editor_from_vars(visual_var, editor_var, path_var)
                .expect("should determine default editor"),
            (
                PathBuf::from("code"),
                vec!["-w", "--reuse-window", "--userdata-dir", "/home/user/code"]
                    .into_iter()
                    .map(String::from)
                    .collect()
            )
        );
    }

    /// Split EDITOR into editor and args
    #[test]
    fn test_determine_editor_from_vars_editor_with_args() {
        let visual_var = "".to_owned();
        let editor_var = "code -w".to_owned();

        let path_var = "".to_owned();

        assert_eq!(
            Edit::determine_editor_from_vars(visual_var, editor_var, path_var)
                .expect("should determine default editor"),
            (
                PathBuf::from("code"),
                vec!["-w"].into_iter().map(String::from).collect()
            )
        );
    }

    /// VISUAL whitespace only defaults to EDITOR before PATH
    #[test]
    fn test_determine_editor_from_vars_visual_whitespace() {
        let visual_var = "       ".to_owned();
        let editor_var = "code -w".to_owned();

        let tmp1 = tempdir().expect("should create tempdir");
        let tmp2 = tempdir().expect("should create tempdir");
        let tmp3 = tempdir().expect("should create tempdir");

        let path_var = std::env::join_paths([&tmp1, &tmp2, &tmp3].map(|d| d.path().to_owned()))
            .expect("should path-join tmpdirs")
            .into_string()
            .expect("should convert paths from OsString to String");

        let nano = tmp1.path().join("nano");
        let vim = tmp2.path().join("vim");
        File::create(nano).expect("should create file");
        File::create(vim).expect("should create file");

        assert_eq!(
            Edit::determine_editor_from_vars(visual_var, editor_var, path_var)
                .expect("should determine default editor"),
            (
                PathBuf::from("code"),
                vec!["-w"].into_iter().map(String::from).collect()
            )
        );

        // ensure tempdir lifetimes do not drop -- require tempdir to exist on fs through the end of the test
        assert!(tmp1.path().is_dir());
        assert!(tmp2.path().is_dir());
        assert!(tmp3.path().is_dir());
    }

    /// EDITOR whitespace only defaults to editors on PATH
    #[test]
    fn test_determine_editor_from_vars_editor_whitespace() {
        let visual_var = "".to_owned();
        let editor_var = "       ".to_owned();

        let tmp1 = tempdir().expect("should create tempdir");
        let tmp2 = tempdir().expect("should create tempdir");
        let tmp3 = tempdir().expect("should create tempdir");

        let path_var = std::env::join_paths([&tmp1, &tmp2, &tmp3].map(|d| d.path().to_owned()))
            .expect("should path-join tmpdirs")
            .into_string()
            .expect("should convert paths from OsString to String");

        let nano = tmp1.path().join("nano");
        let vim = tmp2.path().join("vim");
        File::create(&nano).expect("should create file");
        File::create(vim).expect("should create file");

        assert_eq!(
            Edit::determine_editor_from_vars(visual_var, editor_var, path_var)
                .expect("should determine default editor"),
            (nano, Vec::new())
        );

        // ensure tempdir lifetimes do not drop -- require tempdir to exist on fs through the end of the test
        assert!(tmp1.path().is_dir());
        assert!(tmp2.path().is_dir());
        assert!(tmp3.path().is_dir());
    }

    /// VISUAL and EDITOR whitespace only defaults to editors on PATH
    #[test]
    fn test_determine_editor_from_vars_whitespace() {
        let visual_var = "       ".to_owned();
        let editor_var = "       ".to_owned();

        let tmp1 = tempdir().expect("should create tempdir");
        let tmp2 = tempdir().expect("should create tempdir");
        let tmp3 = tempdir().expect("should create tempdir");

        let path_var = std::env::join_paths([&tmp1, &tmp2, &tmp3].map(|d| d.path().to_owned()))
            .expect("should path-join tmpdirs")
            .into_string()
            .expect("should convert paths from OsString to String");

        let nano = tmp1.path().join("nano");
        let vim = tmp2.path().join("vim");
        File::create(&nano).expect("should create file");
        File::create(vim).expect("should create file");

        assert_eq!(
            Edit::determine_editor_from_vars(visual_var, editor_var, path_var)
                .expect("should determine default editor"),
            (nano, Vec::<String>::new())
        );

        // ensure tempdir lifetimes do not drop -- require tempdir to exist on fs through the end of the test
        assert!(tmp1.path().is_dir());
        assert!(tmp2.path().is_dir());
        assert!(tmp3.path().is_dir());
    }

    /// If no no manifest file or contents are provided,
    /// edits should be blocked if the local checkout is out of sync.
    #[tokio::test]
    async fn edit_requires_sync_checkout() {
        let owner = "owner".parse().unwrap();
        let (flox, _temp_dir_handle) = flox_instance_with_optional_floxhub(Some(&owner));
        let old_contents = indoc! {r#"
            version = 1
        "#};

        let new_contents = indoc! {r#"
            version = 1

            [vars]
            foo = "bar"
        "#};

        let environment = mock_managed_environment_unlocked(&flox, old_contents, owner);

        // edit the local manifest
        fs::write(environment.manifest_path(&flox).unwrap(), new_contents).unwrap();

        let err = Edit::edit_manifest(&flox, &mut ConcreteEnvironment::Managed(environment), None)
            .await
            .expect_err("edit should fail");

        let err = err
            .downcast::<ManagedEnvironmentError>()
            .expect("should be a ManagedEnvironmentError");

        assert!(matches!(err, ManagedEnvironmentError::CheckoutOutOfSync));
    }

    /// If a manifest file or contents are provided, edit succeeds despite local changes.
    #[tokio::test]
    async fn edit_with_file_ignores_local_changes() {
        let owner = "owner".parse().unwrap();
        let (flox, _temp_dir_handle) = flox_instance_with_optional_floxhub(Some(&owner));
        let old_contents = indoc! {r#"
            version = 1
        "#};

        let new_contents = indoc! {r#"
            version = 1

            [vars]
            foo = "bar"
        "#};

        let environment = mock_managed_environment_unlocked(&flox, old_contents, owner);

        // edit the local manifest
        fs::write(environment.manifest_path(&flox).unwrap(), new_contents).unwrap();

        Edit::edit_manifest(
            &flox,
            &mut ConcreteEnvironment::Managed(environment),
            Some(new_contents.to_string()),
        )
        .await
        .expect("edit should succeed");
    }

    /// When the [include] section is modified, a warning is printed
    #[tokio::test]
    async fn edit_warns_when_include_changed() {
        let (flox, tempdir) = flox_instance();
        let (subscriber, writer) = test_subscriber_message_only();

        // Create composer environment
        let composer_path = tempdir.path().join("composer");
        let mut composer_manifest_contents = indoc! {r#"
        version = 1
        "#};
        fs::create_dir(&composer_path).unwrap();
        let composer = new_path_environment_in(&flox, composer_manifest_contents, &composer_path);

        // Create dep environment
        let dep_path = tempdir.path().join("dep");
        let dep_manifest_contents = indoc! {r#"
        version = 1

        [vars]
        foo = "dep"
        "#};

        fs::create_dir(&dep_path).unwrap();
        let mut dep = new_path_environment_in(&flox, dep_manifest_contents, &dep_path);
        dep.lockfile(&flox).unwrap();

        composer_manifest_contents = indoc! {r#"
        version = 1

        [include]
        environments = [
          { dir = "../dep" }
        ]
        "#};
        let composer_new_manifest_path = tempdir.path().join("temporary-manifest.toml");
        fs::write(&composer_new_manifest_path, composer_manifest_contents).unwrap();

        Edit {
            environment: EnvironmentSelect::Dir(composer.parent_path().unwrap()),
            action: EditAction::EditManifest {
                file: Some(composer_new_manifest_path),
            },
        }
        .handle(flox)
        .with_subscriber(subscriber)
        .await
        .unwrap();

        assert_eq!(writer.to_string(), indoc! {"
            ✅ Environment successfully updated.
            ℹ️  Run 'flox list -c' to see merged manifest.
            "});
    }

    #[tokio::test]
    async fn edit_warns_when_fields_overridden() {
        let (flox, tempdir) = flox_instance();
        let (subscriber, writer) = test_subscriber_message_only();

        let mut dep = new_path_environment(&flox, indoc! {r#"
            version = 1

            [vars]
            foo = "dep1"
        "#});
        dep.lockfile(&flox).unwrap();

        // Lock with includes but no top-level overrides yet.
        let composer_original_manifest = formatdoc! {r#"
            version = 1

            [include]
            environments = [
                {{ dir = "{dir}", name = "dep" }},
            ]"#,
            dir = dep.parent_path().unwrap().to_string_lossy(),
        };
        let mut composer = new_path_environment(&flox, &composer_original_manifest);
        composer.lockfile(&flox).unwrap();

        // Edit with top-level overrides.
        let composer_new_manifest = formatdoc! {r#"
            {composer_original_manifest}

            [vars]
            foo = "composer"
        "#};
        let composer_new_manifest_path = tempdir.path().join("temporary-manifest.toml");
        fs::write(&composer_new_manifest_path, composer_new_manifest).unwrap();

        Edit {
            environment: EnvironmentSelect::Dir(composer.parent_path().unwrap()),
            action: EditAction::EditManifest {
                file: Some(composer_new_manifest_path),
            },
        }
        .handle(flox)
        .with_subscriber(subscriber)
        .await
        .unwrap();

        // - overrides are shown even if `includes` didn't change.
        // - hint to see the merged manifest is shown.
        assert_eq!(writer.to_string(), indoc! {"
            ✅ Environment successfully updated.
            ℹ️  The following manifest fields were overridden during merging:
            - This environment set:
              - vars.foo
            ℹ️  Run 'flox list -c' to see merged manifest.
            "});
    }
}
