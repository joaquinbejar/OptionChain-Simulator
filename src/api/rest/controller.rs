use std::sync::Arc;
use actix_web::{web, App, HttpServer};
use crate::api::rest::handlers::{create_session, delete_session, get_next_step, replace_session, update_session};
use crate::session::SessionManager;

// Controller configuration
pub fn configure_routes(cfg: &mut web::ServiceConfig, session_manager: Arc<SessionManager>) {
    cfg.app_data(web::Data::new(session_manager))
        .service(
            web::resource("/api/v1/chain")
                .route(web::post().to(create_session))
                .route(web::get().to(get_next_step))
                .route(web::put().to(replace_session))
                .route(web::patch().to(update_session))
                .route(web::delete().to(delete_session)),
        );
}


pub async fn start_server(session_manager: Arc<SessionManager>) -> std::io::Result<()> {
    HttpServer::new(move || {
        App::new()
            .configure(|cfg| configure_routes(cfg, session_manager.clone()))
    })
        .bind("0.0.0.0:7070")?
        .run()
        .await
}