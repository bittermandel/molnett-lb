use std::{collections::HashMap, net::SocketAddr, str::FromStr, sync::Arc};

use clap::{Parser, ValueEnum};
use error::Error;
use hyper::{
    http::HeaderValue,
    server::conn::AddrStream,
    service::{make_service_fn, service_fn},
    Client, Server, Uri,
};
use rand::Rng;
use tokio::sync::Mutex;

async fn forward_request(
    mut req: hyper::Request<hyper::Body>,
    ip: std::net::IpAddr,
    edge_service_map: Arc<Mutex<HashMap<&str, &str>>>,
    worker_service_map: Arc<Mutex<HashMap<&str, Vec<&str>>>>,
    application_url_map: Arc<Mutex<HashMap<&str, &str>>>,
    mode: ForwardMode,
) -> Result<hyper::Response<hyper::Body>, hyper::Error> {
    let client = if mode == ForwardMode::Edge {
        Client::builder().http2_only(true).build_http()
    } else {
        *req.version_mut() = hyper::Version::HTTP_11;
        Client::builder().build_http()
    };

    println!("{:?}", req);

    let request_uri = if mode == ForwardMode::Edge {
        Uri::from_str(req.headers().get("HOST").unwrap().to_str().unwrap()).unwrap()
    } else {
        Uri::from_str(
            req.headers()
                .get("X-MOLNETT-APPLICATION")
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .unwrap()
    };

    let host = if mode == ForwardMode::Edge {
        edge_service_map
            .lock()
            .await
            .get(ip.to_string().as_str())
            .unwrap()
            .clone()
    } else {
        let worker_service_map = worker_service_map.lock().await;
        let mut rng = rand::thread_rng();
        let index = rng.gen_range(0..worker_service_map.len() + 1);

        worker_service_map.get(request_uri.host().unwrap()).unwrap()[index].clone()
    };

    let uri_string = format!(
        "http://{}{}",
        host,
        req.uri()
            .path_and_query()
            .map(|x| x.as_str())
            .unwrap_or("/")
    );

    println!("Forwarding request to: {}", uri_string);

    let uri = uri_string.parse().unwrap();
    *req.uri_mut() = uri;

    if mode == ForwardMode::Edge {
        let application = application_url_map
            .lock()
            .await
            .get(ip.to_string().as_str())
            .unwrap()
            .clone();

        req.headers_mut().append(
            "X-MOLNETT-APPLICATION",
            HeaderValue::from_str(application).unwrap(),
        );
    }

    let mut res = client.request(req).await.unwrap();

    println!("Protocol of response: {:?}", res.version());

    res.headers_mut()
        .append("X-MOLNETT-WORKER", HeaderValue::from_static("localhost"));

    Ok(res)
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
enum ForwardMode {
    /// Worker mode
    Worker,

    /// Edge mode
    Edge,
}

/// Simple program to greet a person
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Worker ID
    #[arg(short, long, default_value_t = 0)]
    id: u16,

    /// ForwardMode
    #[clap(value_enum)]
    mode: ForwardMode,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    pretty_env_logger::init();

    let args = Args::parse();

    let in_addr: SocketAddr = SocketAddr::from(([127, 0, 0, 1], 8080 + args.id));
    let out_addr = SocketAddr::from(([127, 0, 0, 1], 8080 + 1 + args.id));

    let application_url_map = Arc::new(Mutex::new(HashMap::new()));
    {
        let mut application_url_map = application_url_map.lock().await;
        application_url_map.insert("127.0.0.1", "molnett");
    };

    let service_edge_worker_map = Arc::new(Mutex::new(HashMap::new()));
    {
        let mut service_edge_worker_map = service_edge_worker_map.lock().await;
        service_edge_worker_map.insert("127.0.0.1", "localhost:8081");
    };

    let service_worker_local_map = Arc::new(Mutex::new(HashMap::new()));
    {
        let mut service_worker_local_map = service_worker_local_map.lock().await;
        service_worker_local_map.insert("molnett", vec!["localhost:8082", "localhost:8000"]);
    };

    let make_service = make_service_fn(move |conn: &AddrStream| {
        let ip = conn.local_addr().ip();
        println!("Incoming connection from: {}", ip);

        let service_edge_worker_map = service_edge_worker_map.clone();
        let service_worker_local_map = service_worker_local_map.clone();
        let application_url_map = application_url_map.clone();

        async move {
            // This is the `Service` that will handle the connection.
            // `service_fn` is a helper to convert a function that
            // returns a Response into a `Service`.
            Ok::<_, hyper::Error>(service_fn(move |req| {
                let service_edge_worker_map = service_edge_worker_map.clone();
                let service_worker_local_map = service_worker_local_map.clone();
                let application_url_map = application_url_map.clone();

                forward_request(
                    req,
                    ip,
                    service_edge_worker_map,
                    service_worker_local_map,
                    application_url_map,
                    args.mode,
                )
            }))
        }
    });

    let server = Server::bind(&in_addr).serve(make_service);

    println!("Listening on http://{}", in_addr);
    println!("Proxying on http://{}", out_addr);

    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
    Ok(())
}
