pub mod health;
pub mod routes;

pub use routes::files::files_router;
pub use routes::projects::{projects_router, projects_router_with_clock};
pub use routes::uploads::{uploads_router, uploads_router_with_clock};
