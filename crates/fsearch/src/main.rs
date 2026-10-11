#[cfg(unix)]
mod unix_main;

#[cfg(unix)]
fn main() {
    unix_main::main();
}

#[cfg(not(unix))]
fn main() {}
