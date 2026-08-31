use actix_web::{HttpResponse, Responder, http::header};

/// The icon itself, compiled into the binary.
///
/// It used to be read from `static/favicon.ico` at request time, which made
/// the route depend on the process's working directory. No container image
/// ships that directory, so every containerised deployment answered **500**
/// with `No such file or directory (os error 2)` as the body: a filesystem
/// error handed to whoever asked for a picture.
///
/// Embedding removes the failure rather than handling it. There is no path to
/// get wrong, no working directory to be in, and nothing for a deployment to
/// forget to copy.
const FAVICON: &[u8] = include_bytes!("../../../static/favicon.ico");

/// How long a browser may keep it. The icon changes when the binary does, so
/// a day is safe and saves a request per visitor per day.
const CACHE_CONTROL: &str = "public, max-age=86400";

/// Serves the application icon.
///
/// Always succeeds: the bytes are part of the binary.
pub(crate) async fn get_favicon() -> impl Responder {
    HttpResponse::Ok()
        .content_type("image/x-icon")
        .insert_header((header::CACHE_CONTROL, CACHE_CONTROL))
        .body(FAVICON)
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::body::MessageBody;
    use actix_web::http::StatusCode;

    /// The icon is served, whatever the working directory is.
    ///
    /// The previous version of this test renamed a file on disk to prove the
    /// handler failed without it. That failure is what a deployment actually
    /// met, so the behaviour it asserted is gone and so is the test.
    #[actix_web::test]
    async fn test_the_favicon_is_served_from_the_binary() {
        let response = get_favicon().await;
        let response =
            response.respond_to(&actix_web::test::TestRequest::default().to_http_request());

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("image/x-icon")
        );

        match response.into_body().try_into_bytes() {
            Ok(bytes) => {
                assert!(!bytes.is_empty(), "the icon must carry bytes");
                assert_eq!(bytes.len(), FAVICON.len());
            }
            Err(_) => panic!("the icon body must be readable in one piece"),
        }
    }

    /// It does not depend on where the process was started from, which is the
    /// whole point of the change.
    #[actix_web::test]
    async fn test_the_favicon_does_not_depend_on_the_working_directory() {
        let elsewhere = std::env::temp_dir();
        let original = match std::env::current_dir() {
            Ok(original) => original,
            Err(error) => panic!("the working directory must be readable: {error}"),
        };

        if std::env::set_current_dir(&elsewhere).is_err() {
            // A sandbox that refuses the change cannot exercise this; the
            // assertion above already covers the happy path.
            return;
        }
        let response = get_favicon().await;
        let response =
            response.respond_to(&actix_web::test::TestRequest::default().to_http_request());
        let status = response.status();
        let _ = std::env::set_current_dir(original);

        assert_eq!(
            status,
            StatusCode::OK,
            "the icon must serve from a directory that has no static/ in it"
        );
    }
}
