pub fn create_staging_dir(short_id: &str) -> Result<PathBuf> {
    let path = std::path::PathBuf::from("/tmp/ff-");
    path.push(short_id);
    path.push("/");
    let dir = std::fs::create_dir_all(&path)?;
    env::set_var("FF_STAGING", &path);
    Ok(path)
}
