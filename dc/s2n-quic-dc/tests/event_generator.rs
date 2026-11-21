// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A check-only test: it runs the event generator over the repository's event maps and fails,
//! printing `git diff`, where the generator's output differs from the generated files of this
//! crate. It runs the same command as the `generate-events` job of the `ci` workflow.

use std::{path::Path, process::Command};

#[test]
fn generated_events_match_the_event_generator() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());

    let generator = Command::new(cargo)
        .current_dir(&root)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .args(["run", "--manifest-path", "./tools/event-generator/Cargo.toml"])
        .output()
        .expect("the event generator starts");
    assert!(
        generator.status.success(),
        "the event generator failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&generator.stdout),
        String::from_utf8_lossy(&generator.stderr),
    );

    let stat = Command::new("git")
        .current_dir(&root)
        .args(["diff", "--stat", "--", "dc/s2n-quic-dc/src/event"])
        .output()
        .expect("git diff starts");
    let diff = Command::new("git")
        .current_dir(&root)
        .args(["diff", "--", "dc/s2n-quic-dc/src/event"])
        .output()
        .expect("git diff starts");
    assert!(
        diff.status.success(),
        "git diff failed: {}",
        String::from_utf8_lossy(&diff.stderr)
    );
    let diff = String::from_utf8_lossy(&diff.stdout);
    assert!(
        diff.is_empty(),
        "the generated files differ from the event generator's output\n{}\n{diff}",
        String::from_utf8_lossy(&stat.stdout),
    );
}
