use crate::config;
use crate::config::{ConfigEntry, ConfigHash};
use crate::https::HttpsClient;
use axum::{
    extract::BodyStream,
    http::{uri::Uri, StatusCode},
    http::{Request, Response},
};
use clap::ArgMatches;
use hyper::{
    header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE},
    Body, HeaderMap, Method,
};
use serde_json::Value;
use std::convert::TryFrom;
use std::error::Error;
use std::sync::Arc;
use tokio::sync::RwLock;
use chrono::offset::Utc;

use crate::create_https_client;

type BoxResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Clone, Debug)]
pub struct State {
    pub config_location: String,
    pub config_read: Arc<RwLock<i64>>,
    pub config: Arc<RwLock<ConfigHash>>,
    pub client: HttpsClient,
}

impl State {
    pub async fn new(opts: ArgMatches<'_>) -> BoxResult<Self> {
        // Set timeout
        let timeout: u64 = opts
            .value_of("timeout")
            .unwrap()
            .parse()
            .unwrap_or_else(|_| {
                eprintln!("Supplied timeout not in range, defaulting to 60");
                60
            });

        let client = create_https_client(timeout)?;
        let config_location = opts.value_of("config").unwrap().to_owned();
        let config = config::parse(client.clone(), &config_location).await?;
        let config_read = Utc::now().timestamp();

        Ok(State {
            config_location,
            config: Arc::new(RwLock::new(config)),
            client,
            config_read: Arc::new(RwLock::new(config_read))
        })
    }

    pub async fn get_entry(&mut self, item: &str) -> Option<ConfigEntry> {
        self.renew().await;
        log::debug!("Getting {} from ConfigHash", &item);
        let config = self.config.read().await;
        let entry = config.get(item);
        entry.cloned()
    }

    pub async fn config(&mut self) -> Value {
        self.renew().await;
        let config = self.config.read().await;
        serde_json::to_value(&*config).expect("Cannot convert to JSON")
    }

    pub async fn renew(&mut self) {
        let config_read = self.config_read.read().await;
        let diff = Utc::now().timestamp() - *config_read;
        if diff >= 30 {
            log::debug!("cache has expired, kicking off config reload");
            drop(config_read);
            self.reload().await;
        } else {
            log::debug!("\"cache has not expired, difference is {} seconds\"", diff);
        }
    }

    pub async fn reload(&mut self) {
        let mut config = self.config.write().await;
        let mut config_read = self.config_read.write().await;
        let new_config = match config::parse(self.client.clone(), &self.config_location).await {
            Ok(e) => e,
            Err(e) => {
                log::error!("\"Could not parse config: {}\"", e);
                config.clone()
            }
        };
        let config_read_time = Utc::now().timestamp();
        *config = new_config;
        *config_read = config_read_time;
    }

    pub async fn response(
        &mut self,
        method: Method,
        endpoint: &str,
        path: &str,
        query: Option<String>,
        mut all_headers: HeaderMap,
        payload: Option<BodyStream>,
    ) -> Response<Body> {
        let config_entry = match self.get_entry(endpoint).await {
            Some(e) => e,
            None => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Body::from("{\"error\": \"please specify known endpoint\"}"))
                    .unwrap()
            }
        };

        let path = path.replace(" ", "%20");

        let host_and_path = match query {
            Some(q) => format!("{}{}?{}", config_entry.url, path, q),
            None => format!("{}{}", config_entry.url, path),
        };

        log::debug!("full uri: {}", host_and_path);

        match Uri::try_from(host_and_path) {
            Ok(u) => {
                let body = match payload {
                    Some(p) => {
                        log::debug!("Received body: {:#?}", &p);
                        Body::wrap_stream(p)
                    }
                    None => {
                        log::debug!("Did not receive a body");
                        Body::empty()
                    }
                };

                let mut req = Request::builder()
                    .method(method)
                    .uri(u)
                    .body(body)
                    .expect("request builder");

                // Append to request the headers passed by client
                all_headers.remove(hyper::header::HOST);
                all_headers.remove(hyper::header::USER_AGENT);
                if !all_headers.contains_key(CONTENT_TYPE) {
                    log::debug!("\"Adding content-type: application/json\"");
                    all_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                };
                let headers = req.headers_mut();
                headers.extend(all_headers.clone());

                // Added Basic Auth if username/password exist
                if !config_entry.username.is_empty() {
                    log::debug!("Generating Basic auth");
                    let user_pass = format!("{}:{}", config_entry.username, config_entry.password);
                    let encoded = base64::encode(user_pass);
                    let basic_auth = format!("Basic {}", encoded);
                    let header_basic_auth = match HeaderValue::from_str(&basic_auth) {
                        Ok(a) => a,
                        Err(e) => {
                            log::error!("{{\"error\":\"{}\"", e);
                            return Response::builder()
                                .status(StatusCode::INTERNAL_SERVER_ERROR)
                                .body(Body::from(
                                    "{\"error\": \"Unparsable username and password provided\"}",
                                ))
                                .unwrap();
                        }
                    };
                    headers.insert(AUTHORIZATION, header_basic_auth);
                } else if !config_entry.token.is_empty() {
                    log::debug!("Generating Bearer auth");
                    let basic_auth = format!("Bearer {}", config_entry.token);
                    let header_bearer_auth = match HeaderValue::from_str(&basic_auth) {
                        Ok(a) => a,
                        Err(e) => {
                            log::error!("{{\"error\":\"{}\"", e);
                            return Response::builder()
                                .status(StatusCode::INTERNAL_SERVER_ERROR)
                                .body(Body::from(
                                    "{\"error\": \"Unparsable token provided\"}",
                                ))
                                .unwrap();
                        }
                    };
                    headers.insert(AUTHORIZATION, header_bearer_auth);
                };

                match self.client.clone().request(req).await {
                    Ok(s) => s,
                    Err(e) => {
                        log::error!("{{\"error\":\"{}\"", e);
                        Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .body(Body::from(
                                "{\"error\": \"Error connecting to rest endpoint\"}",
                            ))
                            .unwrap()
                    }
                }
            }
            Err(_) => Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("{\"error\": \"Error parsing uri\"}"))
                .unwrap(),
        }
    }
}
