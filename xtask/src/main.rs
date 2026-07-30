// Copyright 2024 tison <wander4096@gmail.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::path::Path;
use std::process::Command as StdCommand;

use clap::Parser;
use clap::Subcommand;

#[derive(Parser)]
struct Command {
    #[clap(subcommand)]
    sub: SubCommand,
}

impl Command {
    fn run(self) {
        match self.sub {
            SubCommand::Build(cmd) => cmd.run(),
            SubCommand::Check(cmd) => cmd.run(),
            SubCommand::Lint(cmd) => cmd.run(),
            SubCommand::Test(cmd) => cmd.run(),
            SubCommand::TestUi(cmd) => cmd.run(),
        }
    }
}

#[derive(Subcommand)]
enum SubCommand {
    #[clap(about = "Compile workspace packages.")]
    Build(CommandBuild),
    #[clap(about = "Check mea under the feature matrix.")]
    Check(CommandCheck),
    #[clap(about = "Run format and clippy checks.")]
    Lint(CommandLint),
    #[clap(about = "Run unit tests.")]
    Test(CommandTest),
    #[clap(about = "Run UI tests on the MSRV toolchain.")]
    TestUi(CommandTestUi),
}

#[derive(Parser)]
struct CommandBuild {
    #[arg(long, help = "Assert that `Cargo.lock` will remain unchanged.")]
    locked: bool,
}

impl CommandBuild {
    fn run(self) {
        run_command(make_build_cmd(self.locked));
    }
}

#[derive(Parser)]
struct CommandCheck {}

impl CommandCheck {
    fn run(self) {
        let package = mea_package();
        let features = mea_features(&package);

        run_command(make_check_cmd(&[]));
        for feature in &features {
            run_command(make_check_cmd(std::slice::from_ref(feature)));
        }
        if features.len() > 1 {
            run_command(make_check_cmd(&features));
        }
    }
}

#[derive(Parser)]
struct CommandTest {
    #[arg(long, help = "Run tests serially and do not capture output.")]
    no_capture: bool,
}

impl CommandTest {
    fn run(self) {
        let package = mea_package();
        let features = mea_features(&package);
        run_command(make_test_cmd(self.no_capture, &features));
    }
}

#[derive(Parser)]
struct CommandTestUi {
    #[arg(long, help = "Overwrite expected compiler diagnostics.")]
    overwrite: bool,
}

impl CommandTestUi {
    fn run(self) {
        let package = mea_package();
        let features = mea_features(&package);
        let rust_version = package
            .rust_version
            .as_ref()
            .expect("mea must declare its minimum supported Rust version")
            .to_string();

        run_command(make_ui_test_cmd(&features, &rust_version, self.overwrite));
    }
}

fn mea_package() -> cargo_metadata::Package {
    use cargo_metadata::Metadata;
    use cargo_metadata::MetadataCommand;

    let manifest = Path::new(env!("CARGO_WORKSPACE_DIR")).join("Cargo.toml");
    let Metadata { packages, .. } = MetadataCommand::new()
        .manifest_path(manifest)
        .exec()
        .expect("failed to get cargo metadata");

    packages
        .into_iter()
        .find(|package| package.name == "mea")
        .expect("failed to find mea package")
}

fn mea_features(package: &cargo_metadata::Package) -> Vec<String> {
    let mut features = package
        .features
        .keys()
        .filter(|feature| feature.as_str() != "default")
        .cloned()
        .collect::<Vec<_>>();
    features.sort();
    features
}

#[derive(Parser)]
#[clap(name = "lint")]
struct CommandLint {
    #[arg(long, help = "Automatically apply lint suggestions.")]
    fix: bool,
}

impl CommandLint {
    fn run(self) {
        run_command(make_clippy_cmd(self.fix));
        run_command(make_format_cmd(self.fix));
        run_command(make_taplo_cmd(self.fix));
        run_command(make_typos_cmd());
        run_command(make_hawkeye_cmd(self.fix));
    }
}

fn find_command(cmd: &str) -> StdCommand {
    match which::which(cmd) {
        Ok(exe) => {
            let mut cmd = StdCommand::new(exe);
            cmd.current_dir(env!("CARGO_WORKSPACE_DIR"));
            cmd
        }
        Err(err) => {
            panic!("{cmd} not found: {err}");
        }
    }
}

fn ensure_installed(bin: &str, crate_name: &str) {
    if which::which(bin).is_err() {
        let mut cmd = find_command("cargo");
        cmd.args(["install", crate_name]);
        run_command(cmd);
    }
}

fn run_command(mut cmd: StdCommand) {
    println!("{cmd:?}");
    let status = cmd.status().expect("failed to execute process");
    assert!(status.success(), "command failed: {status}");
}

fn make_build_cmd(locked: bool) -> StdCommand {
    let mut cmd = find_command("cargo");
    cmd.args([
        "build",
        "--workspace",
        "--all-features",
        "--tests",
        "--examples",
        "--benches",
        "--bins",
    ]);
    if locked {
        cmd.arg("--locked");
    }
    cmd
}

fn make_test_cmd(no_capture: bool, features: &[String]) -> StdCommand {
    let mut cmd = find_command("cargo");
    cmd.args(["test", "--workspace", "--no-default-features"]);
    for feature in features {
        cmd.arg("--features").arg(format!("mea/{feature}"));
    }
    if no_capture {
        cmd.args(["--", "--nocapture"]);
    }
    cmd
}

fn make_ui_test_cmd(features: &[String], rust_version: &str, overwrite: bool) -> StdCommand {
    let mut cmd = find_command("cargo");
    cmd.arg(format!("+{rust_version}"));
    cmd.args(["test", "--package", "mea", "--no-default-features"]);
    for feature in features {
        cmd.arg("--features").arg(feature);
    }
    cmd.args(["--test", "mutex_ui", "--test", "rwlock_ui"]);
    cmd.args(["--", "--ignored"]);
    if overwrite {
        cmd.env("TRYBUILD", "overwrite");
    }
    cmd
}

fn make_check_cmd(features: &[String]) -> StdCommand {
    let mut cmd = find_command("cargo");
    cmd.env("RUSTFLAGS", "-Dwarnings");
    cmd.args([
        "+nightly",
        "check",
        "--package",
        "mea",
        "--all-targets",
        "--no-default-features",
    ]);
    for feature in features {
        cmd.arg("--features").arg(feature);
    }
    cmd
}

fn make_format_cmd(fix: bool) -> StdCommand {
    let mut cmd = find_command("cargo");
    cmd.args(["+nightly", "fmt", "--all"]);
    if !fix {
        cmd.arg("--check");
    }
    cmd
}

fn make_clippy_cmd(fix: bool) -> StdCommand {
    let mut cmd = find_command("cargo");
    cmd.args([
        "+nightly",
        "clippy",
        "--tests",
        "--all-features",
        "--all-targets",
        "--workspace",
    ]);
    if fix {
        cmd.args(["--allow-staged", "--allow-dirty", "--fix"]);
    } else {
        cmd.args(["--", "-D", "warnings"]);
    }
    cmd
}

fn make_hawkeye_cmd(fix: bool) -> StdCommand {
    ensure_installed("hawkeye", "hawkeye");
    let mut cmd = find_command("hawkeye");
    if fix {
        cmd.args(["format", "--fail-if-updated=false"]);
    } else {
        cmd.args(["check"]);
    }
    cmd
}

fn make_typos_cmd() -> StdCommand {
    ensure_installed("typos", "typos-cli");
    find_command("typos")
}

fn make_taplo_cmd(fix: bool) -> StdCommand {
    ensure_installed("taplo", "taplo-cli");
    let mut cmd = find_command("taplo");
    if fix {
        cmd.args(["format"]);
    } else {
        cmd.args(["format", "--check"]);
    }
    cmd
}

fn main() {
    let cmd = Command::parse();
    cmd.run()
}
