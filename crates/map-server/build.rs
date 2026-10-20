use std::{env, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=BUILD_DATE");

    let build_date = env::var("BUILD_DATE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            Command::new("date")
                .args(["-u", "+%Y-%m-%d"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|value| value.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=MAP_BUILD_DATE={build_date}");
}
