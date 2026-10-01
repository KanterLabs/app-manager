fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "get_snapshot",
            "save_machine",
            "remove_machine",
            "test_machine",
            "discover_workspaces",
            "save_source",
            "remove_source",
            "refresh_releases",
            "install_app",
            "rollback_app",
            "connect_broker",
            "disconnect_broker",
            "refresh_broker",
            "request_build",
        ]),
    ))
    .expect("Cannot generate desktop capability manifest");
}
