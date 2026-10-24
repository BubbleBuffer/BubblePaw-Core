//! Local HTTP server helper shared by runner integration tests.
use axum::Router;

/// Serves `app` on an ephemeral localhost port and returns its base URL and task handle.
pub async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (base, task)
}
