// include_dir! doesn't track files added to the embedded directory on stable.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
