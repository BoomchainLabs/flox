{
  lib,
  pre-commit-hooks,
  symlinkJoin,
  system,
  rustfmt,
  fenix,
  rust-toolchain,
  clang-tools_16,
  makeWrapper,
  bash,
}:
pre-commit-hooks.lib.${system}.run {
  src = builtins.path { path = ./.; };
  default_stages = [
    "manual"
    "pre-push"
  ];
  excludes = [ "test_data" ];
  hooks = {
    nixfmt-rfc-style = {
      enable = true;
    };
    clang-format = {
      enable = true;
      types_or = lib.mkForce [
        "c"
        "c++"
      ];
    };
    rustfmt =
      let
        wrapper = symlinkJoin {
          name = "rustfmt-wrapped";
          paths = [ rustfmt ];
          nativeBuildInputs = [ makeWrapper ];
          postBuild =
            let
              # Use nightly rustfmt
              PATH = lib.makeBinPath [
                fenix.stable.cargo
                rustfmt
              ];
            in
            ''
              wrapProgram $out/bin/cargo-fmt --prefix PATH : ${PATH};
            '';
        };
      in
      {
        enable = true;
        entry = lib.mkForce "${wrapper}/bin/cargo-fmt fmt --all --check --manifest-path 'cli/Cargo.toml' -- --color always";
      };
    clippy = {
      enable = true;
      settings = {
        denyWarnings = true;
        # '--tests': ensure that #[cfg(test)] is linted as well
        # '--workspace': lint all packages in the workspace
        # '--no-deps': don't lint dependencies
        extraArgs = "--tests --workspace --no-deps";
      };
    };
    commitizen = {
      stages = [ "commit-msg" ];
      enable = true;
    };
    # NB: `flox-interpreter` implements these at build time.
    shfmt.enable = false;
    # shellcheck.enable = true; # disabled until we have time to fix all the warnings
  };
  imports = [
    (
      { config, ... }:
      {
        hooks.commitizen-in-ci = {

          description = ''
            This hook checks that the commit messages in the checked range are formatted correctly,
            by checking the range `"$PRE_COMMIT_FROM_REF".."$PRE_COMMIT_TO_REF"` with commitizen.
            This requires pre-commit to be run with the --from-ref and --to-ref arguments.
            Currently, this hook is called only in the 'Nix Git hooks" CI pipeline,
            and is required because the commitizen hook does not work with commit ranges.
          '';

          stages = [ "manual" ];

          entry = ''
            ${bash}/bin/bash -c '
              if [ "$PRE_COMMIT" = "1" ] \
              && [ -n "$PRE_COMMIT_FROM_REF" ] \
              && [ -n "$PRE_COMMIT_TO_REF" ]; then

                # If the hook runs in a merge queue, we only allow Revert prefixes,
                # to avoid merging with (fixup!, squash!, Merge, ..) commits.
                # The $IS_MERGE_QUEUE variable is set by the merge-queue action in CI.
                ALLOWED_PREFIXES_FLAG=""
                ALLOWED_PREFIXES=("Revert")
                if [ "$IS_MERGE_QUEUE" = "1" ]; then
                  ALLOWED_PREFIXES_ESCAPED=$(printf "%q " "''${ALLOWED_PREFIXES[@]}")
                  ALLOWED_PREFIXES_FLAG="--allowed-prefixes $ALLOWED_PREFIXES_ESCAPED"
                fi

                ${config.hooks.commitizen.package}/bin/cz check \
                  $ALLOWED_PREFIXES_FLAG \
                  --rev-range "$PRE_COMMIT_FROM_REF".."$PRE_COMMIT_TO_REF"
              else
                echo "Skipping commitizen check because --from-ref and --to-ref are not set"
                exit 0
              fi
            '
          '';
          enable = true;
          # We're checking the whole range, so we don't need to pass filenames
          pass_filenames = false;
          # Should also check empty commits
          always_run = true;
        };
      }
    )
  ];
  settings = {
    rust.cargoManifestPath = "cli/Cargo.toml";
  };
  tools = {
    # use fenix provided clippy
    clippy = rust-toolchain.clippy;
    cargo = rust-toolchain.cargo;
    clang-tools = clang-tools_16;
  };
}
