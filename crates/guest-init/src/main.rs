#[cfg(target_os = "linux")]
fn main() {
    moniqi_guest_init::boot::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("moniqi-init is the Linux guest's PID 1; build it for aarch64-unknown-linux-musl");
    std::process::exit(2);
}
