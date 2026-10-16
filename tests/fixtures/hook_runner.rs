use std::env;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::thread;
use std::time::Duration;

fn main() {
    let mut payload = String::new();
    std::io::stdin()
        .read_to_string(&mut payload)
        .expect("read hook payload");

    let log_path = env::var_os("LEG_HOOK_TEST_LOG").expect("hook log path");
    let label = env::current_exe()
        .expect("hook executable path")
        .file_stem()
        .expect("hook executable name")
        .to_string_lossy()
        .into_owned();
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .expect("open hook log");
    writeln!(log, "{label}\t{payload}").expect("write hook log");

    match env::var("LEG_HOOK_TEST_MODE").as_deref() {
        Ok("deny") => print!("{{\"decision\":\"deny\",\"reason\":\"fixture policy\"}}"),
        Ok("fail") => {
            eprintln!("fixture hook failure");
            std::process::exit(7);
        }
        Ok("sleep") => loop {
            thread::sleep(Duration::from_secs(60));
        },
        _ => {}
    }
}
