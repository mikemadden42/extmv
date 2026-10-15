//! Runs the language-neutral conformance suite (`tests/run.sh`) against the
//! binary Cargo just built.

use std::process::Command;

#[test]
fn conformance_suite() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/run.sh");
    let output = Command::new("bash")
        .arg(script)
        .arg(env!("CARGO_BIN_EXE_extmv"))
        .output()
        .expect("failed to run tests/run.sh");
    let stdout = String::from_utf8_lossy(&output.stdout);
    print!("{stdout}");
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "conformance suite failed");
}
