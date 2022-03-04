use axum::{
    handler::Handler,
    routing::{any, get, post},
    AddExtensionLayer, Router,
};
use axum_extra::middleware;
use chrono::Local;
use clap::{crate_name, crate_version, App, Arg};
use env_logger::{Builder, Target};
use log::LevelFilter;
use std::future::ready;
use std::io::Write;
use std::net::SocketAddr;
use tower_http::auth::RequireAuthorizationLayer;
use tower_http::trace::TraceLayer;

mod config;
mod error;
mod handlers;
mod https;
mod metrics;
mod path;
mod state;
mod cache;

use crate::metrics::{setup_metrics_recorder, track_metrics};
use handlers::{config, echo, handler_404, health, help, proxy, reload, root, cache, cache_clear};
use https::create_https_client;
use state::State;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let opts = App::new(crate_name!())
        .version(crate_version!())
        .author("Daniel F. <Verticaleap>")
        .about(crate_name!())
        .arg(
            Arg::with_name("port")
                .short("p")
                .long("port")
                .help("Set port to listen on")
                .env("PROXIMA_LISTEN_PORT")
                .default_value("8080")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("timeout")
                .short("t")
                .long("timeout")
                .help("Set default global timeout")
                .default_value("60")
                .env("PROXIMA_TIMEOUT")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("username")
                .short("u")
                .long("username")
                .help("Set required client username")
                .env("PROXIMA_CLIENT_USERNAME")
                .requires("password")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("password")
                .short("p")
                .long("password")
                .help("Set required client password")
                .requires("username")
                .env("PROXIMA_CLIENT_PASSWORD")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("config_username")
                .short("u")
                .long("config_username")
                .help("Set required username for config endpoint")
                .env("PROXIMA_AUTH_USERNAME")
                .requires("config_password")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("config_password")
                .short("p")
                .long("config_password")
                .help("Set required password for config endpoint")
                .env("PROXIMA_AUTH_PASSWORD")
                .requires("config_username")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("config")
                .short("c")
                .long("config")
                .env("PROXIMA_CONFIG")
                .required(true)
                .help("Config file")
                .takes_value(true),
        )
        .get_matches();

    // Initialize log Builder
    Builder::new()
        .format(|buf, record| {
            writeln!(
                buf,
                "{{\"date\": \"{}\", \"level\": \"{}\", \"module\": \"{}\", \"line\": \"{}\", \"log\": {}}}",
                Local::now().format("%Y-%m-%dT%H:%M:%S:%f"),
                record.level(),
                record.module_path().unwrap_or(""),
                record.line().unwrap_or(0u32),
                record.args()
            )
        })
        .target(Target::Stdout)
        .filter_level(LevelFilter::Info)
        .parse_default_env()
        .init();

    // Set port
    let port: u16 = opts.value_of("port").unwrap().parse().unwrap_or_else(|_| {
        eprintln!("specified port isn't in a valid range, setting to 8080");
        8080
    });

    // Create state for axum
    let state = State::new(opts.clone()).await?;

    // Create prometheus handle
    let recorder_handle = setup_metrics_recorder();

    // These should be authenticated
    let closed = Router::new()
        .route("/-/config", get(config))
        .route("/-/reload", post(reload))
        .route("/-/cache", get(cache).delete(cache_clear))
        .route("/:endpoint", get(proxy))
        .route("/:endpoint/*path", any(proxy));

    // These should NOT be authenticated
    let open = Router::new()
        .route("/", get(root))
        .route("/-/health", get(health))
        .route("/-/echo", post(echo))
        .route("/-/help", get(help))
        .route("/-/metrics", get(move || ready(recorder_handle.render())));

    let app = match opts.is_present("username") {
        true => {
            let username = opts
                .value_of("username")
                .expect("Missing username")
                .to_string();
            let password = opts
                .value_of("password")
                .expect("Missing username")
                .to_string();
            Router::new()
                .merge(closed)
                .layer(RequireAuthorizationLayer::basic(&username, &password))
                .merge(open)
                .layer(TraceLayer::new_for_http())
                .route_layer(middleware::from_fn(track_metrics))
                .layer(AddExtensionLayer::new(state))
        }
        false => Router::new()
            .merge(closed)
            .merge(open)
            .layer(TraceLayer::new_for_http())
            .route_layer(middleware::from_fn(track_metrics))
            .layer(AddExtensionLayer::new(state)),
    };

    // add a fallback service for handling routes to unknown paths
    let app = app.fallback(handler_404.into_service());

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("Listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service_with_connect_info::<SocketAddr, _>())
        .await?;

    Ok(())
}
