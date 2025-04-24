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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Test that an error is returned when the favicon file is missing.
    #[tokio::test]
    async fn test_get_favicon_not_found() {
        // Arrange: rename the file out of the way
        let original = PathBuf::from("static/favicon.ico");
        let temp = PathBuf::from("static/favicon_temp.ico");
        fs::rename(&original, &temp).expect("Failed to rename favicon.ico for the test");

        // Act
        let result = get_favicon().await;

        // Assert
        assert!(
            result.is_err(),
            "get_favicon should return an Err when the file is missing"
        );

        // Cleanup: put the file back
        fs::rename(&temp, &original).expect("Failed to restore favicon.ico after the test");
    }
}
