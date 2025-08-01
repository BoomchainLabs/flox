# Flox CLI and Library

## Quick Start
```console
$ cd git clone git@github.com/flox/flox.git;
$ cd flox;
# Enter Dev Shell
$ nix develop;
# Build `flox' and its subsystems
$ just build;
# Run the build
$ ./cli/target/debug/flox --help;
# Run the test suite
$ just test-all;
```

## PR Guidelines

### CLA

- [ ] All commits in a Pull Request are
      [signed](https://docs.github.com/en/authentication/managing-commit-signature-verification/signing-commits)
      and Verified by GitHub or via GPG.
- [ ] As an outside contributor you need to accept the flox
      [Contributor License Agreement](.github/CLA.md) by adding your Git/GitHub
      details in a row at the end of the
      [`CONTRIBUTORS.csv`](.github/CONTRIBUTORS.csv) file by way of the same
      pull request or one done previously.

### CI

CI can only be run against the flox/flox repository - it can't be run on forks.
To run CI on external contributions, a maintainer will have to fetch the branch
for a PR and force push it to the `community-CI` branch in the flox/flox repo.
The maintainer reviewing a PR will run CI after approving the PR.
If you ever need a run triggered, feel free to ping your reviewer!

### Commits

This project follows (tries to),
[conventional commits](https://www.conventionalcommits.org/en/v1.0.0/).

We employ [commitizen](https://commitizen-tools.github.io/commitizen/)
to help to enforce those rules.

**Commit messages that explain the content of the commit are appreciated**

-----

For starters: commit messages should follow the pattern:

```
<type>[optional scope]: <description>

[optional body]

[optional footer(s)]
```

The commit contains the following structural elements,
to communicate the intent of the change:

1. **fix**: a commit of the type `fix` patches a bug in the codebase
   (this correlates with PATCH in Semantic Versioning).
2. **feat**: a commit of the type feat introduces a new feature to the codebase
   (this correlates with MINOR in Semantic Versioning).
3. **BREAKING CHANGE**: a commit that has a footer BREAKING CHANGE:,
   or appends a ! after the type/scope, introduces a breaking API change
   (correlating with MAJOR in Semantic Versioning).
   A BREAKING CHANGE can be part of commits of any type.
4. types other than fix: and feat: are allowed,
   for example @commitlint/config-conventional (based on the Angular convention)
   recommends `build`, `chore`, `ci`, `docs`, `style`, `refactor`, `perf`,
   `test`, and others.
5. footers other than BREAKING CHANGE: <description> may be provided
   and follow a convention similar to git trailer format.

Additional types are not mandated by the Conventional Commits specification,
and have no implicit effect in Semantic Versioning
(unless they include a BREAKING CHANGE).

A scope may be provided to a commit’s type,
to provide additional contextual information
and is contained within parenthesis, e.g., feat(parser): add ability to parse
arrays.

-----

A pre-commit hook will ensure only correctly formatted commit messages are
committed.

You can also run

```console
$ cz c
```

or

```console
$ cz commit
```

to make conforming commits interactively.

## Development

```console
$ nix develop;
```

This sets up an environment with dependencies, rust toolchain, variable
and `pre-commit-hooks`.

In the environment, use [`cargo`](https://doc.rust-lang.org/cargo/)
to build the rust based CLI.

**Note:**

cargo based builds should only be used locally.
Flox must be buildable using `flox` or `nix`.

### Build and Run `flox`

- build and run flox
   ```console
   $ pushd cli;
   $ cargo run -- <args>;
   ```
- build a debug build of flox and all the necessary subsystems
   ```console
   $ just build;
   # builds to ./cli/target/debug/flox
   ```
- run flox unit tests
   ```console
   $ just unit-tests
   ```
- build an optimized release build of flox
   ```console
   $ just build-release
   ```

### Lint and Format `flox`

- format rust code:
  ```console
  $ pushd cli;
  $ cargo fmt
  $ cargo fmt --check # just check
  ```
  The project is formatted using rustfmt and applies custom rules through
  `.rustfmt.toml`.
  A pre-commit hook is set up to check rust file formatting.
- format nix code
  ```console
  $ treefmt -f nix .
  $ treefmt -f nix . --fail-on-change # just check
  ```
  A pre-commit hook is set up to check nix file formatting.
- lint rust
  ```console
  $ pushd cli;
  $ cargo clippy --all
  ```
- lint all files (including for formatting):
  ```console
  $ pre-commit run -a
  ```

### Setting up C/C++ IDE support

For VSCode, it's suggested to use the *C/C++* plugin from *Microsoft*.  This
section refers to settings from that plugin.

- Set your environment to use `c17` and `c++20` standards.
  - For VSCode, this is available under *C++ Configuration*
- *Compile commands* are built with `just build-cdb` and will be placed in the
  root folder as `compile_commands.json`.
  - For VSCode, this is available under the *Advanced* drop down under
  *C++ Configuration* and can be set to `${workspaceFolder}/compile_commands.json`.


### Setting up Rust IDE support

- Install the `rust-analyzer` plugin for your editor
   - See the [official installation instruction](https://rust-analyzer.github.io/manual.html#installation)
     (in the `nix develop` subshell the toolchain will already be provided, you
      can skip right to your editor of choice)
- If you prefer to open your editor at the project root, you'll need to help
  `rust-analyzer` find the rust workspace by configuring the`linkedProjects`
  for `rust-analyzer`.
  In VS Code you can add this: to you `.vscode/settings.json`:
  ```json
  "rust-analyzer.linkedProjects": [
     "${workspaceFolder}/cli/Cargo.toml"
  ]
  ```
- If you want to be able to run and get analytics on impure tests, you need to
  activate the `extra-tests` feature
  In VS Code you can add this: to you `.vscode/settings.json`:
  ```json
  "rust-analyzer.cargo.features": [
     "extra-tests"
  ]
  ```
- If you use `foxundermoon.shell-format` on VS Code make sure to configure editor config support for it:
  ```
   "shellformat.useEditorConfig": true,
  ```

### Activation scripts

Flox activations invoke a series of scripts
which begins with the `activate` script
as maintained in the `assets/activation-scripts` subdirectory.
The process of developing these scripts is highly iterative,
and it can be challenging to follow the sequence of scripts
as invoked in different contexts.

To make debugging easier
we have added invocations of a "tracer" script
to the top and bottom of each file to be sourced.
This script then prints to STDERR
the full path of the file and one of "START" or "END"
depending on its position in the file.
We invoke the "tracer" script in this way
so as to remain compatible with all shells.

The default "tracer" script is "true",
which under normal circumstances will do nothing,
but it can be set to a Flox or user-provided script
by way of the following:

1. set `FLOX_ACTIVATE_TRACE` to a non-empty value

    If `FLOX_ACTIVATE_TRACE` is defined
    but does _not_ refer to the path of an executable file
    then tracing will be performed using the standard
    `activate.d/trace` script included in the Flox environment.

1. set `FLOX_ACTIVATE_TRACE` to the path of a program of your choosing

    Otherwise, if `FLOX_ACTIVATE_TRACE` contains the path of an executable file
    then that will be the program invoked
    at the start and end of each activation script.
    This is useful when studying the effects of activation scripts
    on certain environment variables and shell settings.

To use the tracing facility when testing changes to the activation scripts,
the canonical/authoritative method is to build and activate the full `flox` package:
```
nix build
FLOX_ACTIVATE_TRACE=1 result/bin/flox activate [args]
```

## Testing

### Running all tests
To do a full run of the test suite:
```console
$ just test-cli
```

### Unit tests

Most changes should be unit tested.
If it's possible to test logic with a unit test, a unit test is preferred to any
other kind of test.
Unit tests should be added throughout the Rust code in `./cli`.

Unit tests can be run with `just`:

```console
$ nix develop
$ just impure-tests
$ just impure-tests models::environment::test
```


### Integration tests

Integration tests are written with `bats`.
`expect` can be used for `activate` tests that require testing an interactive
shell,
but in general `expect` should be avoided.
Integration tests are located in the `./cli/tests` folder.

Integration tests currently test:
- CLI flags
- A lot of things that should be unit tests
- Integration (no way!?) with:
  - The nix-daemon
  - The shell
  - github:NixOS/nixpkgs
  - cache.nixos.org
  - external Flox services like FloxHub
  - language ecosystems

Integration tests can be run with `just`:

```console
$ nix develop
$ just integ-tests
```


#### Running tests against the Nix-built flox binary

By default integration tests are run against the development build of `flox`
that's at `./cli/target/debug/flox`.
If you want to verify some behavior of the `flox` binary that's built with Nix,
you can do that as well:

```console
$ just nix-integ-tests
```

#### Tests for `flox containerize`

On macOS these tests require `podman` and a VM created by `podman`.
This VM must be called `flox-containerize-vm` and must be created with certain
directories mounted into it.
See the `podman machine init` call in `./cli/tests/containerize.bats` for the
exact command that's run to create the VM.

For local development you are responsible for creating and starting this VM.
You must also make this the default VM while running tests via
`podman system connection default`.
The test suite will use your host machine's `XDG_DATA_HOME` and
`XDG_CONFIG_HOME` to persist data.

The VM is created and started automatically in CI.

#### Continuous testing
When working on the test you would probably want to run them continuously on
every change. In that case run the following:

```console
$ just integ-tests --watch
```

#### `bats` arguments
You can pass arbitrary flags through to `bats` using a `--` separator.

```console
$ just integ-tests -- -j 4
```
This example tells `bats` to run 4 jobs in parallel.

#### Running subsets of tests
You can specify which tests to run by passing arguments.

##### Running a specific file
In order to run a specific test file, pass the filename relative to the tests directory:
```console
$ just integ-tests usage.bats
```
This example will only run tests in the `cli/tests/usage.bats` file.


##### Running tagged tests
When writing integration tests it's important to add tags to each test to
identify which subsystems the integration test is using.
This makes it easier to target a test run at the subsystem you're working on.

You add tags to a test with a special comment:
```
# bats test_tags=foo,bar,baz
@test "this is the name of my test" {
   run "$FLOX_BIN" --help;
   assert_success;
}
```

You can apply a tag to tests in a file with another special comment, which
applies the tags to all of the tests that come after the comment:
```
# bats file_tags=foo

@test "this is the name of my test" {
   run "$FLOX_BIN" --help;
   assert_success;
}


@test "this is the name of my test" {
   run "$FLOX_BIN" --help;
   assert_success;
}
```

Tags cannot contain whitespace, but may contain `-`, `_`, and `:`, where `:` is
used for namespacing.

The list of tags to use for integration tests is as follows:
- `init`
- `build_env`
- `install`
- `uninstall`
- `activate`
- `push`
- `pull`
- `search`
- `edit`
- `list`
- `delete`
- `upgrade`
- `project_env`
- `managed_env`
- `remote_env`
- `python`, `node`, `go`, `ruby`, etc (anything language specific)

Some of these tags will overlap. For example, the `build_env` tag should be used
any time an environment is built, so there is overlap with `install`,
`activate`, etc.

In order to run tests with a specific tag, you'll pass the `--filter-tags`
option to `bats`:
```console
$ just integ-tests -- --filter-tags activate
```
This example will only run tests tagged with `activate`.
You can use boolean logic and specify the flag multiple times to run specific
subsets of tests.
See the [bats usage documentation](https://bats-core.readthedocs.io/en/stable/usage.html)
for details.

##### Running tests in a Linux container

It's possible to shorten the feedback loop when developing Linux dependent
features on a macOS system by running the tests from a container.

Start a container with the code mounted in so that you can continue to use your normal editor:

    docker run \
        --rm --interactive --tty \
        --volume $(pwd):/mnt --workdir /mnt \
        --name flox-dev \
        nixos/nix

Within the container:

    echo 'experimental-features = nix-command flakes' >> /etc/nix/nix.conf
    nix develop

Outside the container, snapshot the store so that you don't have to download the world next time:

    docker commit flox-dev flox:dev

Within the container, remove any existing MacOS binaries and rebuild for Linux:

    just clean
    just build

Within the container, to avoid `variable $src or $srcs should point to the source` errors per [NixOS/nix#8355](https://github.com/NixOS/nix/issues/8355):

    unset TMPDIR

### Mock catalog responses

Mock catalog responses for use with integration tests are generated by:

- [`test_data/`](test_data/)
- [`cli/mk_data/`](cli/mk_data/)

Mock catalog responses for use with unit tests are generated by running the tests in the presence of a proxy server (`httpmock`) that records the request/response flow into a YAML file in [`unit_test_generated`](test_data/unit_test_generated).

For tests that involve `flox publish` the CLI is configured to make requests to a local instance of `catalog-server`, `floxhub`, etc.
Since those repositories are private, only a Flox employee can produce these recordings.

Instructions for generating the mocks:
- Check out the `floxhub` repo.
- Get the path of the `floxhub_test_users.json` file in the `test_data` folder of the `flox` repo.
- Activate the environment in the `floxhub` repo.
- Run `just catalog-server::serve-for-mocks <path>` in the `floxhub` repo with the path identified above.
- In the `flox` repo run `just gen-data <floxhub repo path>` to generate any missing mocks.
    - To regenerate all unit test mocks, run `just gen-data <floxhub repo path> -f`.

You can also generate subsets of mocks.
See the `Justfile` for the list of commands, in particular `gen-unit-data-for-publish`.

Publish tests interact with the local services in ways that are stateful, so it's very important that the mocks are generated from a clean database.
The database is clean after starting up, which means you can clear it by simply restarting it.
A Justfile command is provided to make this convenient: `just catalog-server::restart-and-watch <test users path>` (run from within the `floxhub` repo environment).

If this process of keeping services from one repo running and clean while generating mocks in another looks brittle and manual, that's because it is.

#### FloxHub <-> Flox test suite relationships

This part you don't need to know, but is helpful to understand.

There are enviroment variables/secrets stored in the `floxhub` repo that can't be made public, but that are required by the `flox` test suite.
We extract these variables at mock generation time by essentially calling `flox activate -d <path> -- bash -c 'echo $VARIABLE'`.
We also need the latest catalog page in the `catalog-server` database, so we use `curl` to query that and store it in the `test_data` folder for the unit tests to read at runtime.
The `catalog-server` can populate itself with test users, which removes the need to talk to Auth0 at mock generation time.
This is accomplished by starting the `floxhub` services with an environment variable set.
The `Justfile` in the `floxhub` repo will take care of that if you follow the instructions in the previous section.

As you can see, there's a lot of cross-talk between the two repos while generating mocks.

#### Secrets warnings from GitHub

The publish test mocks are generated against local services that mock the production services, including pre-signed URLs for uploading during a publish operation.
GitHub flags the AWS credentials in these URLs even though (1) they're temporary, and (2) they only refer to a local service.

When you try to push commits that update these mocks you'll see a big scary warning about secrets.
All you need to do is visit the link GitHub provides for each secret it found and allow "exposing" this secret.
If you don't have permissions to proceed, contact one of the engineers on the Developer Workflows team to get sorted out.

## Man Pages

Unreleased changes to `man` pages are available from the `nix develop` shell but
you will need to restart the shell or call `direnv reload` to pick up new
changes.

## Rust Style Guidelines

- In general, structs should derive `Clone` and `Debug`.

## Merges

Changes should be **squashed and merged** into `main`.

Development is done on branches and merged back to `main`.  Keep your branch
updated by rebasing and resolving conflicts regularly to avoid messy merges.
Merges to `main` should be squashed and *ff-only* back to `main` using GitHub
PRs.  Or, if they represent multiple bigger changes, squashed into multiple
distinct change sets.  Also be sure to run all tests before creating a mergeable
PR (See [above](#testing)).
