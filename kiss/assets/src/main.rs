use std::sync::Arc;

use actix_web::{
    get,
    web::{BytesMut, Data, Path, Payload},
    App, HttpRequest, HttpResponse, HttpServer, Responder,
};
use http_cache_reqwest::{CACacheManager, Cache, CacheMode, HttpCache};
use ipis::{
    futures::StreamExt,
    log::{info, warn},
    logger,
};
use kiss_api::proxy::ProxyConfig;
use reqwest::{
    header::{HeaderName, HOST, ORIGIN, REFERER},
    Client, Method,
};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};

#[get("/")]
async fn index() -> impl Responder {
    HttpResponse::Ok().json("kiss-proxy")
}

#[get("/health")]
async fn health() -> impl Responder {
    HttpResponse::Ok().json("healthy")
}

async fn resolve(
    req: HttpRequest,
    method: Method,
    mut payload: Payload,
    client: Data<ClientWithMiddleware>,
    config: Data<Arc<ProxyConfig>>,
    path: Path<(String, String)>,
) -> impl Responder {
    let (site, path) = path.into_inner();

    // payload is a stream of Bytes objects
    let mut body = BytesMut::new();
    while let Some(chunk) = payload.next().await {
        const MAX_SIZE: usize = 262_144; // max payload size is 256k

        match chunk {
            // limit max size of in-memory payload
            Ok(chunk) if (body.len() + chunk.len()) <= MAX_SIZE => {
                body.extend_from_slice(&chunk);
            }
            Ok(_) => {
                return HttpResponse::Forbidden().body("Overflowed");
            }
            Err(e) => {
                warn!("failed to get bytes: {e}");
                return HttpResponse::Forbidden().body("Err");
            }
        }
    }

    match config.search(&site, &path, req.query_string()) {
        Ok(path) => {
            // TODO: replace `body.to_vec()` into `payload` directly
            let mut builder = client.request(method.clone(), &path).body(body.to_vec());
            for (key, value) in req.headers() {
                if ![
                    HOST,
                    ORIGIN,
                    REFERER,
                    HeaderName::from_static("x-forwarded-host"),
                ]
                .contains(key)
                {
                    builder = builder.header(key, value);
                }
            }

            match builder.send().await {
                Ok(res) => {
                    let content_length = res.content_length();
                    let status = res.status();
                    info!("[{method}] {path:?} => {status}");

                    let mut builder = HttpResponse::build(status);
                    for (key, value) in res.headers() {
                        builder.append_header((key, value));
                    }
                    if let Some(content_length) = content_length {
                        builder.no_chunking(content_length);
                    }

                    // the response is already cached
                    match res.bytes().await {
                        Ok(body) => builder.body(body),
                        Err(e) => HttpResponse::Forbidden()
                            .body(format!("failed to download from url {path:?}: {e}")),
                    }
                }
                Err(e) => {
                    HttpResponse::Forbidden().body(format!("failed to find the url {path:?}: {e}"))
                }
            }
        }
        Err(e) => HttpResponse::Forbidden().body(e.to_string()),
    }
}

#[actix_web::main]
async fn main() {
    async fn try_main() -> ::ipis::core::anyhow::Result<()> {
        // Initialize config
        let addr = "0.0.0.0:8888";
        let config = Arc::new(ProxyConfig::load().await?);

        // Initialize cache client
        let client = ClientBuilder::new(Client::new())
            .with(Cache(HttpCache {
                mode: CacheMode::Default,
                manager: CACacheManager::default(),
                options: None,
            }))
            .build();

        // Start web server
        HttpServer::new(move || {
            App::new()
                .app_data(Data::new(client.clone()))
                .app_data(Data::new(config.clone()))
                .service(index)
                .service(health)
                .route("/{site}/{path:.*}", ::actix_web::web::route().to(resolve))
        })
        .bind(addr)
        .unwrap_or_else(|e| panic!("failed to bind to {addr}: {e}"))
        .shutdown_timeout(5)
        .run()
        .await
        .map_err(Into::into)
    }

    logger::init_once();
    try_main().await.expect("running a server")
}
