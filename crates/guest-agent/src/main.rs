#[cfg(target_os = "linux")]
mod android;
#[cfg(target_os = "linux")]
mod scrcpy;
#[cfg(target_os = "linux")]
mod vsock;

#[cfg(target_os = "linux")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Re-exec target: a clean single-threaded process that sets up namespaces (see android.rs).
    if args.get(1).map(String::as_str) == Some("container") && args.len() == 4 {
        android::container_main(&args[2], &args[3]);
    }
    android::start();
    vsock::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("moniqi-agent runs inside the Linux guest; build it for a *-linux-musl target");
    std::process::exit(2);
}
