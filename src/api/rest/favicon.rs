use actix_files::NamedFile;
use std::error::Error;
use std::path::PathBuf;

/// Asynchronously retrieves the favicon of the application.
///
/// This function attempts to open and return the favicon file located
/// at the path `static/favicon.ico`. If the file is successfully found,
/// it is returned wrapped as a `NamedFile`. Otherwise, an error is returned.
///
/// # Returns
/// * `Ok(NamedFile)` - The favicon file wrapped in a `NamedFile` if found.
/// * `Err(Box<dyn Error>)` - An error wrapped in a `Box` if the file
///   could not be opened or does not exist.
///
/// # Errors
/// This function will return an error if:
/// - The `static/favicon.ico` path does not exist or is inaccessible.
/// - Any issues occur during the file opening process.
///
pub(crate) async fn get_favicon() -> Result<NamedFile, Box<dyn Error>> {
    let path: PathBuf = "static/favicon.ico".into();
    Ok(NamedFile::open(path)?)
}
