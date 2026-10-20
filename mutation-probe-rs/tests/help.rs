// Each bin's --help is its manual; a change to it lands as a change to these files.

use std::process::Command;

fn help(bin: &str) -> String {
    let out = Command::new(bin).arg("--help").output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn mutation_gen_help_is_the_manual() {
    assert_eq!(
        help(env!("CARGO_BIN_EXE_mutation-gen")),
        include_str!("help/mutation-gen.txt")
    );
}

#[test]
fn mutation_probe_help_is_the_manual() {
    assert_eq!(
        help(env!("CARGO_BIN_EXE_mutation-probe")),
        include_str!("help/mutation-probe.txt")
    );
}
