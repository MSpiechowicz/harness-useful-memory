#[tauri::command]
pub fn validate_paths(paths: Vec<String>) -> Vec<String> {
    paths.into_iter().filter(|path| !path.is_empty()).collect()
}

#[tauri::command]
pub fn open_project(path: String) -> String {
    path
}
