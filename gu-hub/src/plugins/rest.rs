use actix::Arbiter;
use actix::System;
use actix::SystemService;
use actix_web::error::ErrorBadRequest;
use actix_web::error::ErrorInternalServerError;
use actix_web::http;
use actix_web::AsyncResponder;
use actix_web::HttpMessage;
use actix_web::HttpRequest;
use actix_web::HttpResponse;
use actix_web::Responder;
use actix_web::Scope;
use bytes::buf::IntoBuf;
use bytes::Bytes;
use futures::future;
use futures::future::Future;
use futures::stream::Stream;
use plugins::manager::ChangePluginState;
use plugins::manager::InstallDevPlugin;
use plugins::manager::InstallPlugin;
use plugins::manager::ListPlugins;
use plugins::manager::PluginFile;
use plugins::manager::PluginManager;
use plugins::manager::QueriedState;
use plugins::plugin::format_plugins_table;
use plugins::plugin::PluginInfo;
use server::ServerClient;
use std::io::Cursor;
use std::path::{Path, PathBuf};

pub fn list_query() {
    System::run(|| {
        Arbiter::spawn(
            ServerClient::get("/plug")
                .and_then(|r: Vec<PluginInfo>| Ok(format_plugins_table(r)))
                .map_err(|e| error!("{}", e))
                .then(|_r| Ok(System::current().stop())),
        )
    });
}

pub fn install_query(_path: &Path) {
    /*  System::run(|| {
        Arbiter::spawn(
            ServerClient::post("/plug")
                .and_then(|r: Vec<PluginInfo>| Ok(format_plugins_table(r)))
                .map_err(|e| error!("{}", e))
                .then(|_r| Ok(System::current().stop())),
        )
    });*/
}

pub fn dev_query(path: PathBuf) {
    let path = path
        .canonicalize()
        .expect("Cannot canonicalize dir path")
        .to_str()
        .expect("Cannot parse filepath to str")
        .to_string();

    System::run(move || {
        Arbiter::spawn(
            ServerClient::get(format!("/plug/dev{}", path))
                .and_then(|r: ()| Ok(()))
                .map_err(|e| error!("{}", e))
                .then(|_r| Ok(System::current().stop())),
        )
    });
}

pub fn scope<S: 'static>(scope: Scope<S>) -> Scope<S> {
    scope
        .route("", http::Method::GET, list_scope)
        .route("", http::Method::POST, install_scope)
        .route("/dev/{pluginPath:.*}", http::Method::POST, dev_scope)
        .route("/{pluginName}/activate", http::Method::POST, |r| {
            state_scope(QueriedState::Activate, r)
        }).route("/{pluginName}/inactivate", http::Method::POST, |r| {
            state_scope(QueriedState::Inactivate, r)
        }).route("/{pluginName}/{fileName:.*}", http::Method::GET, file_scope)
}

fn list_scope<S>(_r: HttpRequest<S>) -> impl Responder {
    use actix_web::AsyncResponder;
    let manager = PluginManager::from_registry();

    manager
        .send(ListPlugins)
        .map_err(|e| ErrorInternalServerError(format!("err: {}", e)))
        .and_then(|res| Ok(HttpResponse::Ok().json(res)))
        .responder()
}

enum ContentType {
    JavaScript,
    Html,
    Svg,
    NotSupported,
}

impl<'a> From<&'a str> for ContentType {
    fn from(s: &'a str) -> Self {
        match s {
            "js" => ContentType::JavaScript,
            "html" => ContentType::Html,
            "svg" => ContentType::Svg,
            _ => ContentType::NotSupported,
        }
    }
}

impl ToString for ContentType {
    fn to_string(&self) -> String {
        match self {
            ContentType::JavaScript => "application/javascript".to_string(),
            ContentType::Html => "text/html".to_string(),
            ContentType::Svg => "image/svg+xml".to_string(),
            ContentType::NotSupported => "Content type not supported".to_string(),
        }
    }
}

fn file_scope<S>(r: HttpRequest<S>) -> impl Responder {
    let manager = PluginManager::from_registry();
    let match_info = r.match_info();

    let path = PathBuf::from(
        match_info
            .get("fileName")
            .expect("Can't get filename from query"),
    );

    let plugin = match_info
        .get("pluginName")
        .expect("Can't get plugin name from query")
        .to_string();

    let b = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|a| ContentType::from(a));

    match b {
        None => future::err(ErrorBadRequest("Cannot parse file extension")).responder(),
        Some(ContentType::NotSupported) => {
            future::err(ErrorBadRequest(ContentType::NotSupported.to_string())).responder()
        }
        Some(content) => manager
            .send(PluginFile { plugin, path })
            .map_err(|e| ErrorInternalServerError(format!("err: {}", e)))
            .and_then(|res| res.map_err(|e| ErrorInternalServerError(format!("err: {}", e))))
            .and_then(move |res| {
                Ok(HttpResponse::Ok()
                    .content_type(content.to_string())
                    .body(res))
            }).responder(),
    }
}

fn install_scope<S>(r: HttpRequest<S>) -> impl Responder {
    let manager = PluginManager::from_registry();

    r.payload()
        .map_err(|e| ErrorBadRequest(format!("Couldn't get request body: {:?}", e)))
        .concat2()
        .and_then(|a| Ok(a.into_buf()))
        .and_then(move |a: Cursor<Bytes>| {
            manager
                .send(InstallPlugin { bytes: a })
                .map_err(|e| ErrorInternalServerError(format!("{:?}", e)))
        }).and_then(|_| Ok(HttpResponse::Ok()))
        .responder()
}

fn state_scope<S>(state: QueriedState, r: HttpRequest<S>) -> impl Responder {
    let manager = PluginManager::from_registry();
    let match_info = r.match_info();

    let plugin = match_info
        .get("pluginName")
        .expect("Can't get plugin name from query")
        .to_string();

    manager
        .send(ChangePluginState { plugin, state })
        .and_then(move |res| Ok(HttpResponse::Ok()))
        .responder()
}

fn dev_scope<S>(r: HttpRequest<S>) -> impl Responder {
    let manager = PluginManager::from_registry();
    let match_info = r.match_info();

    let path = PathBuf::from(format!(
        "/{}",
        match_info
            .get("pluginPath")
            .expect("Can't get plugin name from query")
    ));

    manager
        .send(InstallDevPlugin { path })
        .and_then(move |res| Ok(HttpResponse::Ok()))
        .responder()
}
