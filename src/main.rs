//! # `webdav-server` is a webdav server that handles user-accounts.
//!
//! This is a webdav server that allows access to a users home directory,
//! just like an ancient FTP server would (remember those?).
//!
//! Right now, this server does not implement TLS or logging. The general idea
//! is that most people put a reverse-proxy in front of services like this
//! anyway, like NGINX, that can do TLS and logging.
//!
#![feature(async_await, await_macro, futures_api)]

#[macro_use] extern crate clap;
#[macro_use] extern crate log;
#[macro_use] extern crate lazy_static;
#[macro_use] extern crate serde_derive;

mod cache;
mod cached;
mod config;
mod rootfs;
mod suid;
mod unixuser;
mod userfs;

use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::process::exit;
use std::sync::Arc;

use bytes::Bytes;
use env_logger;
use futures::{future, Future, Stream};
use futures03::{FutureExt, TryFutureExt};
use futures03::compat::Future01CompatExt;
use hyper::{self, service::make_service_fn, server::conn::AddrStream};
use http;
use http::status::StatusCode;
use net2;
use tokio;

use pam_sandboxed::PamAuth;
use webdav_handler::typed_headers::{HeaderMapExt, Authorization, Basic};
use webdav_handler::{DavConfig, DavHandler, fs::DavFileSystem, webpath::WebPath};
use webdav_handler::{ls::DavLockSystem, localfs::LocalFs, fakels::FakeLs};

use crate::userfs::UserFs;
use crate::rootfs::RootFs;
use crate::suid::switch_ugid;

static PROGNAME: &'static str = "webdav-server";

pub type BoxedByteStream = Box<futures::Stream<Item = Bytes, Error = io::Error> + Send + 'static>;

// Contains "state" and a handle to the config.
#[derive(Clone)]
struct Server {
    dh:             DavHandler,
    pam_auth:       PamAuth,
    users_path:     Arc<Option<String>>,
    config:         Arc<config::Config>,
}

#[allow(dead_code)]
type HyperResult = Result<hyper::Response<hyper::Body>, io::Error>;

// Server implementation.
impl Server {

    // Constructor.
    pub fn new(config: Arc<config::Config>, auth: PamAuth) -> Self {

        // empty handler.
        let dh = DavHandler::new_with(DavConfig::default());

        // base path of the users.
        let users_path = match config.users {
            Some(ref users) => {
                if users.path.contains(":username") {
                    Some(users.path.replace(":username", ""))
                } else {
                    None
                }
            },
            None => None,
        };

        Server{
            dh:             dh,
            pam_auth:       auth,
            config:         config,
            users_path:     Arc::new(users_path),
        }
    }

    // get locksystem. FIXME: check user-agent header.
    fn locksystem(&self) -> Option<Box<DavLockSystem>> {
        match self.config.webdav.locksystem.as_str() {
            ""|"fakels" => Some(FakeLs::new()),
            _ => None,
        }
    }

    // get the user path from config.users.path.
    fn user_path(&self, user: &str) -> String {
        match self.config.users {
            Some(ref users) => {
                // replace :user with the username.
                users.path.replace(":username", user)
            },
            None => {
                // something that can never match.
                "-".to_string()
            },
        }
    }

    // check if this is the root filesystem.
    fn is_realroot(&self, uri: &http::uri::Uri) -> Option<(WebPath, bool)>
    {
        // is a rootfs configured?
        let rootfs = match self.config.rootfs {
            Some(ref rootfs) => rootfs,
            None => return None,
        };

        // check prefix.
        let webpath = match WebPath::from_uri(uri, &rootfs.path) {
            Ok(path) => path,
            Err(_) => return None,
        };

        // only files one level deep.
        let nseg = webpath.num_segments();
        if nseg > 1 || (nseg == 1 && webpath.is_collection()) {
            return None;
        }

        Some((webpath, rootfs.auth))
    }

    // futures 0.1 adapter.
    fn handle(&self, req: hyper::Request<hyper::Body>, remote_ip: SocketAddr)
        -> impl Future<Item=hyper::Response<hyper::Body>, Error=io::Error> + Send + 'static
    {
        // NOTE: we move the body out of the request, and pass it seperatly.
        //
        // That is needed since parts from the request can be borrowed,
        // e.g. when you pass req.uri() to  a function. The async/await/futures stuff in
        // the compiler then needs the Request to be Sync, and the body isn't.
        //
        //         error[E0277]: `ReqBody` cannot be shared between threads safely
        //   --> src/main.rs:26:12
        //    |
        // 26 |         -> impl Future<Item=http::Response<()>, Error=std::io::Error> + Send + 'a
        //    |            ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
        //    |            `ReqBody` cannot be shared between threads safely
        //    |
        //    = help: within `http::request::Request<ReqBody>`, the trait `std::marker::Sync` is
        //      not implemented for `ReqBody`
        //    = help: consider adding a `where ReqBody: std::marker::Sync` bound
        //    = note: required because it appears within the type `http::request::Request<ReqBody>`
        //    = note: required because of the requirements on the impl of `std::marker::Send`
        //      for `&http::request::Request<ReqBody>`
        //
        let (parts, body) = req.into_parts();
        let req = http::Request::from_parts(parts, ());
        let self2 = self.clone();
        async move {
            await!(self2.handle_async(req, body, remote_ip))
        }.boxed().compat()
    }

    // authenticate user.
    async fn auth<'a>(&'a self, req: &'a http::Request<()>, remote_ip: Option<&'a str>) -> Result<Arc<unixuser::User>, StatusCode>
    {
        // we must have a login/pass
        let (user, pass) = match req.headers().typed_get::<Authorization<Basic>>() {
            Some(Authorization(Basic{
                                username,
                                password: Some(password)
                            }
            )) => (username, password),
            _ => return Err(StatusCode::UNAUTHORIZED),
        };

        // check if user exists.
        let pwd = match await!(cached::unixuser(&user)) {
            Ok(pwd) => pwd,
            Err(_) => return Err(StatusCode::UNAUTHORIZED),
        };

        // authenticate.
        let service = self.config.pam.service.as_str();
        let pam_auth = self.pam_auth.clone();
        if let Err(_) = await!(cached::pam_auth(pam_auth, service, &pwd.name, &pass, remote_ip)) {
            return Err(StatusCode::UNAUTHORIZED);
        }

        // check minimum uid
        if let Some(min_uid) = self.config.unix.min_uid {
            if pwd.uid < min_uid {
                debug!("Server::auth: {}: uid {} too low (<{})", pwd.name, pwd.uid, min_uid);
                return Err(StatusCode::UNAUTHORIZED);
            }
        }
        Ok(pwd)
    }

    // handle a request.
    async fn handle_async(&self, req: http::Request<()>, body: hyper::Body, remote_ip: SocketAddr) -> HyperResult
    {
        // stringify the remote IP address.
        let ip = remote_ip.ip();
        let ip_string = if ip.is_loopback() {
            // if it's loopback, take the value from the x-real-ip
            // header, if present.
            req.headers().get("x-real-ip").and_then(|s| s.to_str().ok()).map(|s| s.to_owned())
        } else {
            Some(match ip {
                IpAddr::V4(ip) => ip.to_string(),
                IpAddr::V6(ip) => ip.to_string(),
            })
        };
        let ip_ref = ip_string.as_ref().map(|s| s.as_str());

        // see if this is a request for the root filesystem.
        let method = req.method();
        if method == &http::Method::GET || method == &http::Method::HEAD {
            if let Some((webpath, do_auth)) = self.is_realroot(req.uri()) {
                debug!("handle_async: {:?}: handle as realroot", req.uri());
                if do_auth {
                    if let Err(status) = await!(self.auth(&req, ip_ref)) {
                        return await!(self.error(status));
                    }
                }
                return await!(self.handle_realroot(req, body, webpath));
            }
            debug!("handle_async: {:?}: not realroot", req.uri());
        }

        // Normalize the path.
        let path = match WebPath::from_uri(req.uri(), "") {
            Ok(path) => path.as_utf8_string_with_prefix(),
            Err(_) => return await!(self.error(StatusCode::BAD_REQUEST)),
        };

        // Could be a request for the virtual root.
        if let Some(ref users_path) = self.users_path.as_ref() {
            if path.trim_end_matches('/') == users_path.trim_end_matches('/') {
                let pwd = match await!(self.auth(&req, ip_ref)) {
                    Ok(pwd) => pwd,
                    Err(status) => return await!(self.error(status)),
                };
                debug!("handle_async: {:?}: handle as virtualroot", req.uri());
                return await!(self.handle_virtualroot(req, body, pwd));
            }
        }

        // is this the users part of the path?
        let prefix = self.user_path("");
        if !path.starts_with(&prefix) {
            debug!("handle_async: {}: doesn't match start with {}", path, prefix);
            return await!(self.error(StatusCode::NOT_FOUND));
        }

        // authenticate now.
        let pwd = match await!(self.auth(&req, ip_ref)) {
            Ok(pwd) => pwd,
            Err(status) => return await!(self.error(status)),
        };

        // Check if username matches basedir.
        let prefix = self.user_path(&pwd.name);
        if !path.starts_with(&prefix) {
            // in /<something>/ but doesn't match /:user/
            debug!("Server::handle: user {} prefix {} path {} -> 401", pwd.name, prefix, path);
            return await!(self.error(StatusCode::UNAUTHORIZED));
        }

        // All set.
        await!(self.handle_user(req, body, prefix, pwd))
    }

    async fn error(&self, code: StatusCode) -> HyperResult {
        let msg = format!("<error>{} {}</error>\n",
                          code.as_u16(), code.canonical_reason().unwrap_or(""));
        let mut response = hyper::Response::builder();
        response.status(code);
        response.header("Content-Type", "text/xml");
        if code == StatusCode::UNAUTHORIZED {
            let realm = self.config.accounts.realm.as_ref().map(|s| s.as_str()).unwrap_or("Webdav Server");
            response.header("WWW-Authenticate", format!("Basic realm=\"{}\"", realm).as_str());
        }
        Ok(response.body(msg.into()).unwrap())
    }

    async fn redirect(&self, path: String) -> HyperResult {
        let resp = hyper::Response::builder()
            .status(302)
            .header("content-type", "text/plain")
            .header("location", path)
            .body("302 Moved\n".into()).unwrap();
        Ok(resp)
    }

    // serve from the local filesystem.
    async fn handle_realroot(&self, req: http::Request<()>, body: hyper::Body, webpath: WebPath) -> HyperResult
    {
        // get filename.
        let mut webpath = webpath;
        let mut filename = match std::str::from_utf8(webpath.file_name()) {
            Ok(n) => n,
            Err(_) => return await!(self.error(StatusCode::NOT_FOUND)),
        };

        let rootfs = self.config.rootfs.as_ref().unwrap();
        debug!("Server::handle_realroot: serving {:?}", req.uri());

        // index.html?
        let mut req = req;
        if filename == "" {
            let index = rootfs.index.as_ref().map(|s| s.as_str()).unwrap_or("index.html");
            filename = index;
            webpath.push_segment(index.as_bytes());
            let path = webpath.as_url_string_with_prefix();
            if let Ok(pq) = http::uri::PathAndQuery::from_shared(path.into()) {
                let mut parts = req.uri().clone().into_parts();
                parts.path_and_query = Some(pq);
                *req.uri_mut() = http::uri::Uri::from_parts(parts).unwrap();
            }
        }

        // see if file exists.
        let fs = LocalFs::new(&rootfs.directory, true);
        if await!(fs.metadata(&webpath)).is_err() {
            // it doesn't. see if it matches a valid username.
            match await!(cached::unixuser(&filename)) {
                Ok(_) => {
                    debug!("Server::handle_realroot: redirect to /{}/", filename);
                    return await!(self.redirect("/".to_string() + &filename + "/"));
                },
                Err(_) => return await!(self.error(StatusCode::NOT_FOUND)),
            }
        }

        // serve.
        let config = DavConfig {
            fs:         Some(fs),
            ..DavConfig::default()
        };
        await!(self.run_davhandler(req, body, config))
    }

    // virtual root filesytem for PROPFIND/OPTIONS in "/".
    async fn handle_virtualroot(&self, req: http::Request<()>, body: hyper::Body, pwd: Arc<unixuser::User>)
        -> HyperResult
    {
        debug!("Server::handle_virtualroot: /");
        let ugid = match self.config.accounts.setuid {
            true => Some((pwd.uid, pwd.gid)),
            false => None,
        };
        let user = pwd.name.clone();

        let mut methods = webdav_handler::AllowedMethods::none();
        methods.add(webdav_handler::Method::Head);
        methods.add(webdav_handler::Method::Get);
        methods.add(webdav_handler::Method::PropFind);
        methods.add(webdav_handler::Method::Options);

        let prefix = self.users_path.as_ref().clone().unwrap();

        let fs = RootFs::new(&pwd.dir, user.clone(), ugid);
        let config = DavConfig {
            fs:         Some(fs),
            ls:         self.locksystem(),
            prefix:     Some(prefix),
            principal:  Some(user),
            allow:      Some(methods),
            ..DavConfig::default()
        };
        await!(self.run_davhandler(req, body, config))
    }

    async fn handle_user(&self, req: http::Request<()>, body: hyper::Body, prefix: String, pwd: Arc<unixuser::User>)
        -> HyperResult
    {
        // do we have a users section?
        let _users = match self.config.users {
            Some(ref users) => users,
            None => return await!(self.error(StatusCode::NOT_FOUND)),
        };

        let ugid = match self.config.accounts.setuid {
            true => Some((pwd.uid, pwd.gid)),
            false => None,
        };
        let fs = UserFs::new(&pwd.dir, ugid, true);

        debug!("Server::handle_user: in userdir {} prefix {} ", pwd.name, prefix);
        let config = DavConfig {
            prefix:     Some(prefix),
            fs:         Some(fs),
            ls:         self.locksystem(),
            principal:  Some(pwd.name.to_string()),
            ..DavConfig::default()
        };
        await!(self.run_davhandler(req, body, config))
    }

    async fn run_davhandler(&self, req: http::Request<()>, body: hyper::Body, config: DavConfig)
        -> HyperResult
    {
        // move body back into request.
        let (parts, _) = req.into_parts();
        let req = http::Request::from_parts(parts, body.map(|item| Bytes::from(item)));

        // run handler, then transform http::Response into hyper::Response.
        let resp = await!(self.dh.handle_with(config, req).compat())?;
        let (parts, body) = resp.into_parts();
        let body = hyper::Body::wrap_stream(body);
        Ok(hyper::Response::from_parts(parts, body))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // command line option processing.
    let matches = clap_app!(webdav_server =>
        (version: "0.1")
        (@arg CFG: -c --config +takes_value "configuration file (/etc/webdav-server.toml)")
        (@arg PORT: -p --port +takes_value "listen to this port on localhost only")
        (@arg DIR: -d --dir +takes_value "override local directory to serve")
    ).get_matches();

    let dir = matches.value_of("DIR");
    let port = matches.value_of("PORT");
    let cfg = matches.value_of("CFG").unwrap_or("/etc/webdav-server.toml");

    // read config.
    let mut config = match config::read(cfg.clone()) {
        Err(e) => {
            eprintln!("{}: {}: {}", PROGNAME, cfg, e);
            exit(1);
        },
        Ok(c) => c,
    };
    config::check(cfg.clone(), &config);

    // override parts of the config with command line options.
    if let Some(dir) = dir {
        if config.rootfs.is_none() {
            eprintln!("{}: [rootfs] section missing", cfg);
            exit(1);
        }
        config.rootfs.as_mut().unwrap().directory = dir.to_owned();
    }
    if let Some(port) = port {
        let localhosts = vec![
            ("127.0.0.1:".to_string() + port).parse::<SocketAddr>().unwrap(),
            ("[::]:".to_string() + port).parse::<SocketAddr>().unwrap(),
        ];
        config.server.listen = config::OneOrManyAddr::Many(localhosts);
    }
    let config = Arc::new(config);

    // set cache timeouts.
    if let Some(timeout) = config.pam.cache_timeout {
        cached::set_pamcache_timeout(timeout);
    }
    if let Some(timeout) = config.unix.cache_timeout {
        cached::set_pwcache_timeout(timeout);
    }

    // resolve addresses.
    let addrs = match config.server.listen.clone().to_socket_addrs() {
        Err(e) => {
            eprintln!("{}: [server] listen: {:?}", cfg, e);
            exit(1);
        },
        Ok(a) => a,
    };

    // get pam task and handle, get a runtime, and start the pam task.
    let (pam, pam_task) = PamAuth::lazy_new(config.pam.threads.clone())?;
    let mut rt = tokio::runtime::Runtime::new()?;
    rt.spawn(pam_task.map_err(|_e| debug!("pam_task returned error {}", _e)));

    // start servers (one for each listen address).
    let dav_server = Server::new(config.clone(), pam);
    let mut servers = Vec::new();
    for sockaddr in addrs {
        let listener = match make_listener(&sockaddr) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{}: listener on {:?}: {}", PROGNAME, &sockaddr, e);
                exit(1);
            },
        };
        let dav_server = dav_server.clone();
        let make_service = make_service_fn(move |socket: &AddrStream| {
            let dav_server = dav_server.clone();
            let remote_addr = socket.remote_addr();
            hyper::service::service_fn(move |req| {
                dav_server.handle(req, remote_addr)
            })
        });
        println!("Listening on http://{:?}", sockaddr);
        let server = hyper::Server::from_tcp(listener)?
            .serve(make_service)
            .map_err(|e| eprintln!("server error: {}", e));
        servers.push(server);
    }

    // drop privs.
    match (&config.server.uid, &config.server.gid) {
        (&Some(uid), &Some(gid)) => switch_ugid(uid, gid),
        _ => {},
    }

    // run all servers and wait for them to finish.
    let servers = future::join_all(servers).then(|_| Ok::<_, hyper::Error>(()));
    let _ = rt.block_on_all(servers);

    Ok(())
}

// Make a new TcpListener, and if it's a V6 listener, set the
// V6_V6ONLY socket option on it.
fn make_listener(addr: &SocketAddr) -> io::Result<std::net::TcpListener> {
    let s = if addr.is_ipv6() {
        let s = net2::TcpBuilder::new_v6()?;
        s.only_v6(true)?;
        s
    } else {
        net2::TcpBuilder::new_v4()?
    };
    s.reuse_address(true)?;
    s.bind(addr)?;
    s.listen(128)
}

