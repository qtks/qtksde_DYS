fn main() {
    // The Windows manifest declares longPathAware so >260 character paths keep
    // working after the index hands them to Explorer / shell APIs.
    let attributes = tauri_build::Attributes::new().windows_attributes(
        tauri_build::WindowsAttributes::new().app_manifest(include_str!("windows-app.manifest")),
    );
    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}
