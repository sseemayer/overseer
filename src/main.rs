use std::{collections::HashMap, future::IntoFuture, sync::Arc};

use anyhow::Result;
use axum::{extract::State, routing::get, Json, Router};
use dashmap::DashMap;
use docker_api::{
    models::ContainerSummary,
    opts::{ContainerFilter, ContainerListOpts},
    Docker,
};
use futures::{join, StreamExt};
use serde::Serialize;
use tower_http::trace::{self, TraceLayer};
use tracing::{debug, info};
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

#[derive(OpenApi)]
#[openapi(
        paths(
            get_services,
        ),
        components(
            schemas(ServicesResponse, ServiceInfo)
        ),
        tags(
            (name = "services", description = "Service enumeration API")
        )
    )]
struct ApiDoc;

#[derive(Debug, Clone, Serialize, ToSchema)]
struct ServicesResponse {
    services: HashMap<String, ServiceInfo>,
}

#[utoipa::path(
    get,
    path = "/services",
    responses(
        (status = 200, description = "Currently-running services", body = ServicesResponse, example = json!(
            ServicesResponse { 
                services: vec![
                    ("5033dd90804f4fccb1f66fd011d90f3713be66486c642770e6cf6fa9ccacf1c2".to_string(), ServiceInfo {
                        values: vec![
                            ("name".to_string(), "My Awesome Service".to_string()),
                            ("description".to_string(), "An example service description".to_string()),
                            ("url".to_string(), "https://myservice.ndim.space".to_string()),
                        ].into_iter().collect()
                    })
                ].into_iter().collect()
            }

        ))

    )
)]
async fn get_services(state: State<Arc<Store>>) -> Json<ServicesResponse> {
    let services = state
        .services
        .iter()
        .map(|r| (r.key().to_owned(), r.value().to_owned()))
        .collect();

    Json(ServicesResponse { services })
}

#[derive(Debug, Clone, Default)]
struct Store {
    services: DashMap<String, ServiceInfo>,
}

impl Store {
    async fn reload_from_docker(&self, docker: &Docker) -> Result<()> {
        self.services.clear();

        let clo = ContainerListOpts::builder().all(true).build();

        for container in docker.containers().list(&clo).await? {
            if let Some(state) = &container.state {
                if state != "running" {
                    continue;
                }
            } else {
                continue;
            }

            let id = container.id.to_owned().unwrap_or_default();
            let si = ServiceInfo::from_container_summary(&container);

            if si.values.is_empty() { continue; }

            self.services.insert(id, si);
        }

        Ok(())
    }

    async fn update_service(&self, docker: &Docker, id: &str) -> Result<()> {
        let clo = ContainerListOpts::builder()
            .filter(vec![ContainerFilter::Id(id.to_string())])
            .build();

        for container in docker.containers().list(&clo).await? {
            let id = container.id.to_owned().unwrap_or_default();
            let si = ServiceInfo::from_container_summary(&container);

            if si.values.is_empty() { continue; }

            self.services.insert(id, si);
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, ToSchema)]
struct ServiceInfo {
    #[serde(flatten)]
    values: HashMap<String, String>,
}

impl ServiceInfo {
    fn from_container_summary(container: &ContainerSummary) -> Self {
        let mut values = HashMap::new();

        if let Some(labels) = &container.labels {
            for (key, value) in labels {
                if !key.starts_with("overseer.") {
                    continue;
                }

                let key = key.trim_start_matches("overseer.").to_string();
                let value = value.to_string();

                values.insert(key, value);
            }
        }

        ServiceInfo { values }
    }
}

async fn handle_events(docker: &Docker, store: &Store) -> Result<()> {
    while let Some(event) = docker.events(&Default::default()).next().await {
        let event = event?;

        let action = event.action.as_ref().map(|v| &v[..]).unwrap_or("");

        if let Some(id) = event.actor.as_ref().and_then(|a| a.id.clone()) {
            match action {
                "start" => {
                    info!("Container with ID {} started", id);
                    store.update_service(docker, &id).await?;
                }
                "stop" | "kill" => {
                    info!("Container with ID {} {}ed", id, action);
                    store.services.remove(&id);
                }

                _ => debug!("Ignoring '{}' event {:?}", action, event),
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let bind_uri = std::env::var("OVERSEER_BIND_URI").unwrap_or("0.0.0.0:3000".to_string());
    let docker_connection =
        std::env::var("OVERSEER_DOCKER_URI").unwrap_or("unix:///var/run/docker.sock".to_string());
    let docker = Docker::new(&docker_connection)?;

    let state = Arc::new(Store::default());
    state.reload_from_docker(&docker).await?;

    info!(
        "Loaded {} services from {}",
        state.services.len(),
        docker_connection
    );

    // build our application with a single route
    let app = Router::new()
        .merge(SwaggerUi::new("/api").url("/openapi.json", ApiDoc::openapi()))
        .route("/services", get(get_services))
        .with_state(state.clone())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(trace::DefaultMakeSpan::new().level(tracing::Level::INFO))
                .on_response(trace::DefaultOnResponse::new().level(tracing::Level::INFO)),
        );

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind(&bind_uri).await.unwrap();

    info!("Listening on {}", bind_uri);

    let (r_a, r_b) = join!(
        axum::serve(listener, app).into_future(),
        handle_events(&docker, state.as_ref()).into_future(),
    );

    r_a?;
    r_b?;

    Ok(())
}
