//! Makes the topmost "== V<x.y.z>" heading of README.adoc the app version (`WHISPERER_VERSION`),
//! so the exe always reports the same version as the release the workflow publishes for it, and
//! embeds a manifest for the modern look of the naming dialog's controls (Common Controls 6).
fn main() {
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'");
    println!("cargo:rerun-if-changed=README.adoc");
    let readme = std::fs::read_to_string("README.adoc").expect("read README.adoc");
    let version = readme
        .lines()
        .find_map(|l| l.strip_prefix("== V"))
        .and_then(|rest| rest.split_whitespace().next())
        .filter(|v| v.split('.').count() == 3 && v.split('.').all(|p| p.parse::<u32>().is_ok()))
        .expect("README.adoc needs a '== V<x.y.z> - ...' heading");
    println!("cargo:rustc-env=WHISPERER_VERSION={version}");
}
